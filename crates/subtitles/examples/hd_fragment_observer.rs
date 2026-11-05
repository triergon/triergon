#![forbid(unsafe_code)]
//! Private source-only correctness observer. Never reads native expectations.
use std::{fmt::Write as _, io::Read, sync::atomic::AtomicBool};
use triergon_core::{Budget, Bytes, Rounding, Time, TimeBase, WorkBudget};
use triergon_subtitles::dvd::{
    ConfigJob, ConfigLimits, ConfigStatus, DvdBitmap, HdFacts, HdFragmentIntegrity,
    HdFragmentLimits, HdFragmentState, HdFragmentStatus, HdPacketClock, HdPhysicalPacket, HdReset,
    HdSpan, HdStatus, NativeCropMode, NativeProjectionJob, ProjectionLimits, ProjectionStatus,
};
type Result<T> = std::result::Result<T, String>;
const MAX_INPUT: usize = 16 * 1024 * 1024;
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or("protocol offset overflow")?;
        let bytes = self.data.get(self.at..end).ok_or("truncated protocol")?;
        self.at = end;
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| "u32 protocol")?,
        ))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| "i64 protocol")?,
        ))
    }
}

fn hex(raw: &[u8]) -> String {
    let mut text = String::with_capacity(raw.len() * 2);
    for byte in raw {
        write!(text, "{byte:02x}").expect("String formatting");
    }
    text
}
fn quoted(raw: &str) -> String {
    let mut out = String::from("\"");
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                write!(out, "\\u{:04x}", u32::from(c)).expect("String formatting");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn optional(value: Option<i64>) -> String {
    value.map_or_else(|| "null".into(), |v| v.to_string())
}

fn microseconds(known: bool, pts: i64, timebase: TimeBase) -> Option<i64> {
    // Public AVPacket API aliases this source value with its unknown sentinel.
    // The source record independently retains both the raw value and known bit.
    if !known || pts == i64::MIN {
        return None;
    }
    Time::from_ticks(i128::from(pts), timebase)
        .ok()?
        .to_i64_ticks(TimeBase::new(1, 1_000_000).ok()?, Rounding::NearestAway)
        .ok()
}

fn span_json(span: Option<HdSpan>, raw: &[u8]) -> String {
    span.map_or_else(
        || "null".into(),
        |s| {
            format!(
                "{{\"range\":[{},{}],\"raw_hex\":\"{}\"}}",
                s.start,
                s.end,
                hex(s.get(raw).expect("source span"))
            )
        },
    )
}
fn facts_json(f: HdFacts, raw: &[u8], indices: Option<&[u8]>) -> String {
    let area = f.area.map_or_else(
        || "null".into(),
        |a| format!("[{},{},{},{}]", a.left, a.right, a.top, a.bottom),
    );
    let fields = f.fields.map_or_else(
        || "null".into(),
        |a| {
            format!(
                "{{\"top_raw\":{},\"bottom_raw\":{},\"operand_bits\":{}}}",
                a.top, a.bottom, a.operand_bits
            )
        },
    );
    format!(
        "{{\"ycrcb\":{},\"contrast\":{},\"area\":{},\"area_wire\":{},\"fields\":{},\"fields_wire\":{},\"eight_bit\":{},\"forced\":{},\"menu\":{},\"start_date_raw\":{},\"stop_date_raw\":{},\"forced_date_raw\":{},\"palette4_word\":{},\"palette4_wire\":{},\"alpha4_word\":{},\"alpha4_wire\":{},\"last_opaque_tail\":{},\"stopped_at_link\":{},\"source_rgb\":null,\"source_plane_decoded\":{},\"source_indices_hex\":\"{}\"}}",
        span_json(f.ycrcb, raw),
        span_json(f.contrast, raw),
        area,
        span_json(f.area_wire, raw),
        fields,
        span_json(f.fields_wire, raw),
        f.eight_bit,
        f.forced,
        f.menu,
        optional(f.start_date.map(|d| i64::from(d.0))),
        optional(f.stop_date.map(|d| i64::from(d.0))),
        optional(f.forced_date.map(|d| i64::from(d.0))),
        optional(f.palette4_word.map(i64::from)),
        span_json(f.palette4_wire, raw),
        optional(f.alpha4_word.map(i64::from)),
        span_json(f.alpha4_wire, raw),
        span_json(f.last_opaque_tail, raw),
        span_json(f.stopped_at_link, raw),
        indices.is_some(),
        hex(indices.unwrap_or(&[]))
    )
}
#[allow(clippy::too_many_arguments)]
fn render<const N: usize>(
    bitmap: DvdBitmap,
    palette: [u32; N],
    facts: HdFacts,
    pool: &Budget,
    quantum: u64,
    cancel: &AtomicBool,
    packet: u32,
    length: usize,
    clock: Option<i64>,
    canvas: [u32; 2],
) -> Result<(Option<String>, usize)> {
    let mut projection = NativeProjectionJob::new(
        bitmap,
        palette,
        if facts.menu {
            NativeCropMode::Menu
        } else {
            NativeCropMode::Ordinary
        },
        pool,
        ProjectionLimits::default(),
    )
    .map_err(|e| format!("{e:?}"))?;
    while projection
        .poll(&mut WorkBudget::new(quantum), cancel)
        .map_err(|e| format!("{e:?}"))?
        != ProjectionStatus::Ready
    {}
    let projection = projection
        .try_finish(cancel)
        .map_err(|e| format!("{e:?}"))?;
    let Some(view) = projection.native_view() else {
        return Ok((None, 0));
    };
    let crop = view.crop();
    let count = usize::from(crop.width) * usize::from(crop.height);
    let mut pixels = String::new();
    for row in 0..crop.height {
        pixels.push_str(&hex(view.indices_row(row).map_err(|e| format!("{e:?}"))?));
    }
    let words = view
        .palette()
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let raw_palette = view
        .palette()
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect::<Vec<_>>();
    let start = facts.start_date.map_or(0, |d| d.ticks_90k() / 90);
    let stop = facts.stop_date.map_or(0, |d| d.ticks_90k() / 90);
    let event = format!(
        concat!(
            "{{\"kind\":\"subtitle\",\"packet\":{},\"packet_offset\":0,\"consumed\":{},\"flush\":false,\"format\":0,",
            "\"pts_microseconds\":{},\"start_display_ms\":{},\"end_display_ms\":{},\"context_canvas\":[{},{}],",
            "\"rectangles\":[{{\"order\":0,\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"flags\":{},",
            "\"native_stride\":{},\"indices_hex\":\"{}\",\"palette_entries\":{},\"raw_native_palette_hex\":\"{}\",\"palette_argb_u32\":[{}]}}]}}"
        ),
        packet,
        length,
        optional(clock),
        start,
        stop,
        canvas[0],
        canvas[1],
        crop.area.left,
        crop.area.top,
        crop.width,
        crop.height,
        u8::from(facts.forced),
        crop.width,
        pixels,
        N,
        hex(&raw_palette),
        words
    );
    Ok((Some(event), count))
}
// Whole-file case-envelope validation precedes every decoder/config operation.
// Empty physical packets are legal only in the explicitly versioned V3 input.
struct PhysicalSource<'a> {
    reset: u8,
    known: u8,
    pts: i64,
    bytes: &'a [u8],
}
fn parse_physical_sources<'a>(
    input: &mut Cursor<'a>,
    packets: u32,
    v3: bool,
    private_len: usize,
) -> Result<(Vec<PhysicalSource<'a>>, bool)> {
    let mut records = Vec::new();
    let mut refusal = false;
    let mut total = private_len;
    for _ in 0..packets {
        let reset = if v3 { input.take(1)?[0] } else { 0 };
        let known = input.take(1)?[0];
        let pts = input.i64()?;
        let len = input.u32()? as usize;
        if reset > 2 || known > 1 {
            return Err("source reset/known bit".into());
        }
        total = total.checked_add(len).ok_or("source overflow")?;
        if total > MAX_INPUT {
            return Err("source byte limit".into());
        }
        let bytes = input.take(len)?;
        refusal |= !v3 && bytes.is_empty();
        records.push(PhysicalSource {
            reset,
            known,
            pts,
            bytes,
        });
    }
    if input.at != input.data.len() {
        return Err("trailing protocol bytes".into());
    }
    Ok((records, refusal))
}

fn run(raw: &[u8], quantum: u64) -> Result<String> {
    if quantum == 0 {
        return Err("positive quantum required".into());
    }
    let mut input = Cursor { data: raw, at: 0 };
    let magic = input.take(8)?;
    let v3 = magic == b"SMDVC003";
    if !v3 && magic != b"SMDVC002" {
        return Err("source protocol".into());
    }
    let private_len = input.u32()? as usize;
    if private_len > 65536 {
        return Err("private limit".into());
    }
    let private = input.take(private_len)?;
    let packets = input.u32()?;
    let numerator = input.u32()?;
    let denominator = input.u32()?;
    if packets == 0
        || packets > 64
        || numerator == 0
        || denominator == 0
        || numerator > i32::MAX as u32
        || denominator > i32::MAX as u32
    {
        return Err("protocol count/timebase".into());
    }
    let (records, input_refusal) = parse_physical_sources(&mut input, packets, v3, private_len)?;
    let sources = records.iter().enumerate().map(|(packet, record)| {
        format!("{{\"packet\":{packet},\"reset_before\":{},\"known\":{},\"pts_raw\":{},\"payload_hex\":\"{}\"}}", record.reset, record.known != 0, record.pts, hex(record.bytes))
    }).collect::<Vec<_>>();
    if input_refusal {
        // C3 rejects this outer input before opening a decoder. Preserve source
        // facts, but do not receive packets, finish input, or fabricate a drain.
        return Ok(format!(
            "{{\"private_hex\":\"{}\",\"timebase\":[{},{}],\"source_packets\":[{}],\"events\":[],\"extended_source\":[],\"decode_calls\":[],\"drain_calls\":[],\"fragment_dispositions\":[],\"incomplete_at_end\":null,\"source_rejections\":[],\"source_program_complete\":false,\"modeled_protocol_input_refusal\":true,\"errors\":[],\"C3_reference_adoption_pending\":true,\"full_decoder_qualification\":false}}",
            hex(private),
            numerator,
            denominator,
            sources.join(",")
        ));
    }
    let tb = TimeBase::new(u64::from(numerator), u64::from(denominator))
        .map_err(|e| format!("{e:?}"))?;
    let pool = Budget::new(64 * 1024 * 1024).map_err(|e| format!("{e:?}"))?;
    let cancel = AtomicBool::new(false);
    let mut config = ConfigJob::new(
        Bytes::copy_from_slice(&pool, private).map_err(|e| format!("{e:?}"))?,
        &pool,
        ConfigLimits::default(),
    )
    .map_err(|e| format!("{e:?}"))?;
    while config
        .poll(&mut WorkBudget::new(quantum), &cancel)
        .map_err(|e| format!("{e:?}"))?
        != ConfigStatus::Ready
    {}
    let config = config.try_finish(&cancel).map_err(|e| format!("{e:?}"))?;
    let canvas = config.canvas.map_or([720, 480], |c| [c.width, c.height]);
    let mut state = Some(
        HdFragmentState::new(&pool, HdFragmentLimits::default()).map_err(|e| format!("{e:?}"))?,
    );
    let mut events = Vec::new();
    let mut facts_out = Vec::new();
    let mut calls = Vec::new();
    let mut dispositions = Vec::new();
    let mut rejections = Vec::new();
    let mut source_complete = true;
    for (packet, record) in records.into_iter().enumerate() {
        let packet = u32::try_from(packet).map_err(|_| "packet order overflow")?;
        let PhysicalSource {
            reset,
            known,
            pts,
            bytes,
        } = record;
        let len = bytes.len();
        let physical = HdPhysicalPacket {
            bytes: Bytes::copy_from_slice(&pool, bytes).map_err(|e| format!("{e:?}"))?,
            clock: HdPacketClock {
                known: known != 0,
                raw: pts,
                timebase: tb,
            },
            order: u64::from(packet),
            reset: match reset {
                0 => HdReset::Keep,
                1 => HdReset::Flush,
                _ => HdReset::Reopen,
            },
        };
        let mut job =
            HdFragmentState::start(&mut state, physical).map_err(|e| format!("{:?}", e.error))?;
        while job
            .poll(&mut WorkBudget::new(quantum), &cancel)
            .map_err(|e| format!("{e:?}"))?
            != HdFragmentStatus::Ready
        {}
        let (next, mut out) = job.try_finish(&cancel).map_err(|e| format!("{e:?}"))?;
        state = Some(next);
        let history=out.history.stamps().map(|s|format!("{{\"order\":{},\"known\":{},\"pts_raw\":{},\"timebase\":[{},{}],\"range\":[{},{}]}}",s.order,s.clock.known,s.clock.raw,numerator,denominator,s.range.start,s.range.end)).collect::<Vec<_>>().join(",");
        let discarded = out.reset_discarded.as_ref().map_or("null".into(), |p| {
            format!(
                "{{\"raw_hex\":\"{}\",\"fragments\":{}}}",
                hex(&p.bytes),
                p.history.len()
            )
        });
        let supplied = out
            .supplied_source
            .as_ref()
            .map_or("null".into(), |p| format!("\"{}\"", hex(p)));
        if matches!(out.integrity, HdFragmentIntegrity::Truncated { .. }) {
            rejections.push(format!("{{\"packet\":{packet},\"source_error\":\"Truncated\",\"receiver_prefix_recovery\":true}}"));
        }
        let mut event = None;
        if matches!(out.integrity, HdFragmentIntegrity::PhysicalPrefixConsumed) && len != 0 {
            source_complete = false;
        }
        if matches!(
            out.integrity,
            HdFragmentIntegrity::Trailing { .. } | HdFragmentIntegrity::Truncated { .. }
        ) {
            source_complete = false;
        }
        if let Some(frame) = out.frame.take() {
            let f = frame.source().source_facts();
            let p = frame.source();
            let parsed = hex(p.packet());
            facts_out.push(format!("{{\"packet\":{packet},\"assembled_source_hex\":\"{parsed}\",\"ended_at_input\":{},\"facts\":{}}}",f.ended_at_input,facts_json(f,p.packet(),p.picture().map(|p|p.bitmap().pixels().as_ref()))));
            if let Some(error) = p.source_error() {
                source_complete = false;
                rejections.push(format!(
                    "{{\"packet\":{packet},\"source_error\":{}}}",
                    quoted(&format!("{error:?}"))
                ));
            }
            if f.ended_at_input || f.last_opaque_tail.is_some() || f.stopped_at_link.is_some() {
                source_complete = false;
            }
            if p.picture().is_some() {
                let (source, image_state, _next_state) = frame.into_parts();
                let (_, picture, _, _) = source.into_parts();
                let picture = picture.ok_or("picture absent")?;
                let clock = microseconds(known != 0, pts, tb);
                let mut native_facts = f;
                native_facts.forced = picture.native_forced();
                if f.eight_bit {
                    if picture.ycrcb().is_some() {
                        let mut palette = image_state
                            .palette_job(&picture)
                            .map_err(|e| format!("{e:?}"))?;
                        while palette
                            .poll(&mut WorkBudget::new(quantum), &cancel)
                            .map_err(|e| format!("{e:?}"))?
                            != HdStatus::Ready
                        {}
                        let palette = palette.try_finish(&cancel).map_err(|e| format!("{e:?}"))?;
                        let (_, bitmap, _) = picture.into_parts();
                        event = render(
                            bitmap,
                            palette,
                            native_facts,
                            &pool,
                            quantum,
                            &cancel,
                            packet,
                            out.projected_returned,
                            clock,
                            canvas,
                        )?
                        .0;
                    }
                } else {
                    let palette = image_state
                        .four_palette(config.palette.as_ref())
                        .map_err(|e| format!("{e:?}"))?;
                    let (_, bitmap, _) = picture.into_parts();
                    event = render(
                        bitmap,
                        palette.argb,
                        native_facts,
                        &pool,
                        quantum,
                        &cancel,
                        packet,
                        out.projected_returned,
                        clock,
                        canvas,
                    )?
                    .0;
                }
            }
        }
        calls.push(format!("{{\"kind\":\"decode-call\",\"packet\":{packet},\"offset\":0,\"supplied\":{len},\"returned\":{},\"got\":{}}}",out.projected_returned,u8::from(event.is_some())));
        dispositions.push(format!("{{\"packet\":{packet},\"integrity\":{},\"history\":[{history}],\"supplied_source_hex\":{supplied},\"reset_discarded\":{discarded}}}",quoted(&format!("{:?}",out.integrity))));
        if let Some(event) = event {
            events.push(event);
        }
    }
    if input.at != raw.len() {
        return Err("trailing protocol bytes".into());
    }
    let mut end = state.take().ok_or("end state absent")?.end();
    while end
        .poll(&mut WorkBudget::new(quantum), &cancel)
        .map_err(|e| format!("{e:?}"))?
        != HdFragmentStatus::Ready
    {}
    let (_state, end) = end.try_finish(&cancel).map_err(|e| format!("{e:?}"))?;
    let unfinished = end.incomplete_at_end.map_or("null".into(), |p| {
        source_complete = false;
        format!(
            "{{\"source_error\":\"Truncated\",\"raw_hex\":\"{}\",\"fragments\":{}}}",
            hex(&p.bytes),
            p.history.len()
        )
    });
    Ok(format!(
        "{{\"private_hex\":\"{}\",\"timebase\":[{},{}],\"source_packets\":[{}],\"events\":[{}],\"extended_source\":[{}],\"decode_calls\":[{}],\"drain_calls\":[{{\"kind\":\"drain-call\",\"call\":0,\"returned\":0,\"got\":0}}],\"fragment_dispositions\":[{}],\"incomplete_at_end\":{},\"source_rejections\":[{}],\"source_program_complete\":{},\"modeled_protocol_input_refusal\":{},\"errors\":[],\"C3_reference_adoption_pending\":true,\"full_decoder_qualification\":false}}",
        hex(private),
        numerator,
        denominator,
        sources.join(","),
        events.join(","),
        facts_out.join(","),
        calls.join(","),
        dispositions.join(","),
        unfinished,
        rejections.join(","),
        source_complete,
        input_refusal
    ))
}
fn main() {
    let result = (|| {
        let mut a = std::env::args().skip(1);
        let path = a.next().ok_or("source file required")?;
        let quantum: u64 = a
            .next()
            .ok_or("quantum required")?
            .parse()
            .map_err(|_| "quantum")?;
        if a.next().is_some() {
            return Err("unexpected argument".into());
        }
        let f = std::fs::File::open(path).map_err(|e| format!("{e}"))?;
        let mut data = Vec::new();
        f.take((MAX_INPUT + 65536 + 1025) as u64)
            .read_to_end(&mut data)
            .map_err(|e| format!("{e}"))?;
        run(&data, quantum)
    })();
    match result {
        Ok(line) => println!("{line}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod refusal_contract_tests {
    use super::run;
    fn source(magic: &[u8; 8], private: &[u8], packets: &[&[u8]]) -> Vec<u8> {
        let mut raw = magic.to_vec();
        raw.extend_from_slice(&(private.len() as u32).to_be_bytes());
        raw.extend_from_slice(private);
        raw.extend_from_slice(&(packets.len() as u32).to_be_bytes());
        raw.extend_from_slice(&1u32.to_be_bytes());
        raw.extend_from_slice(&90_000u32.to_be_bytes());
        for packet in packets {
            if magic == b"SMDVC003" {
                raw.push(0);
            }
            raw.push(1);
            raw.extend_from_slice(&90_000i64.to_be_bytes());
            raw.extend_from_slice(&(packet.len() as u32).to_be_bytes());
            raw.extend_from_slice(packet);
        }
        raw
    }
    #[test]
    fn v2_empty_input_refusal_has_no_receive_or_drain() {
        for quantum in [1, 4096] {
            let output =
                run(&source(b"SMDVC002", b"", &[b""]), quantum).expect("modeled protocol refusal");
            assert!(output.contains("\"modeled_protocol_input_refusal\":true"));
            assert!(output.contains("\"decode_calls\":[]"));
            assert!(output.contains("\"drain_calls\":[]"), "{output}");
        }
    }
    #[test]
    fn v2_refusal_precedes_configuration_and_prefix_decode() {
        for quantum in [1, 4096] {
            // The wrapper must reject all input before parsing even invalid
            // codec configuration or receiving any earlier nonempty prefix.
            let output = run(
                &source(b"SMDVC002", b"palette: invalid\n", &[b"prefix", b""]),
                quantum,
            )
            .expect("protocol refusal must precede decoder setup");
            assert!(output.contains("\"modeled_protocol_input_refusal\":true"));
            assert!(output.contains("\"decode_calls\":[]"));
            assert!(output.contains("\"drain_calls\":[]"));
            assert!(output.contains("\"payload_hex\":\"707265666978\""));
        }
    }
    #[test]
    fn v3_empty_physical_packet_keeps_receive_and_drain() {
        for quantum in [1, 4096] {
            let output =
                run(&source(b"SMDVC003", b"", &[b""]), quantum).expect("V3 permits empty packets");
            assert!(output.contains("\"modeled_protocol_input_refusal\":false"));
            assert!(output.contains("\"decode_calls\":[{\"kind\":\"decode-call\",\"packet\":0,\"offset\":0,\"supplied\":0,\"returned\":0,\"got\":0}]"));
            assert!(output.contains(
                "\"drain_calls\":[{\"kind\":\"drain-call\",\"call\":0,\"returned\":0,\"got\":0}]"
            ));
        }
    }
}
