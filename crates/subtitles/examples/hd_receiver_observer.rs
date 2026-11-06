#![forbid(unsafe_code)]
//! Private source-only correctness observer. Never reads native expectations.
use std::{fmt::Write as _, io::Read, sync::atomic::AtomicBool};
use triergon_core::{Budget, Bytes, Rounding, Time, TimeBase, WorkBudget};
use triergon_harness_protocol::hex;
use triergon_subtitles::dvd::{
    ConfigJob, ConfigLimits, ConfigStatus, DvdBitmap, HdContextJob, HdContextOrigin, HdFacts,
    HdLimits, HdReceiverContext, HdReceiverStatus, HdSpan, HdStatus, NativeCropMode,
    NativeProjectionJob, ProjectionLimits, ProjectionStatus,
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
fn context_json(context: &HdReceiverContext, owners: &[Bytes]) -> Result<String> {
    fn origin(value: HdContextOrigin<'_>, owners: &[Bytes]) -> Result<String> {
        match value {
            HdContextOrigin::ReceiverReset => Ok("{\"kind\":\"receiver-reset\"}".into()),
            HdContextOrigin::Contrast { owner, span } | HdContextOrigin::Packed { owner, span } => {
                let packet = owners
                    .iter()
                    .position(|p| p.shares_storage_with(owner))
                    .ok_or("context source owner absent")?;
                Ok(format!(
                    "{{\"kind\":\"source\",\"packet\":{packet},\"range\":[{},{}],\"raw_hex\":\"{}\"}}",
                    span.start,
                    span.end,
                    hex(span.get(owner).map_err(|e| format!("{e:?}"))?)
                ))
            }
        }
    }
    let values = (0..=255)
        .map(|i| {
            context
                .alpha(i)
                .map(|a| a.value.to_string())
                .map_err(|e| format!("{e:?}"))
        })
        .collect::<Result<Vec<_>>>()?
        .join(",");
    let selectors = context.selectors().map_err(|e| format!("{e:?}"))?;
    Ok(format!(
        "{{\"effective_alpha\":[{values}],\"selectors\":[{},{},{},{}],\"alpha_first4_origin\":{},\"alpha_full_origin\":{},\"selectors_origin\":{}}}",
        selectors[0],
        selectors[1],
        selectors[2],
        selectors[3],
        origin(
            context.alpha(0).map_err(|e| format!("{e:?}"))?.origin,
            owners
        )?,
        origin(
            context.alpha(255).map_err(|e| format!("{e:?}"))?.origin,
            owners
        )?,
        origin(context.selectors_origin(), owners)?
    ))
}

fn run(raw: &[u8], quantum: u64) -> Result<String> {
    if quantum == 0 {
        return Err("positive quantum required".into());
    }
    let mut input = Cursor { data: raw, at: 0 };
    if input.take(8)? != b"SMDVC002" {
        return Err("protocol identity".into());
    }
    let private_len = usize::try_from(input.u32()?).map_err(|_| "private narrowing")?;
    if private_len > 65536 {
        return Err("private limit".into());
    }
    let private = input.take(private_len)?;
    let packets = input.u32()?;
    let numerator = input.u32()?;
    let denominator = input.u32()?;
    if packets > 64 {
        return Err("packet limit".into());
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
    let canvas = config.canvas.map_or([0, 0], |c| [c.width, c.height]);
    let mut source = Vec::new();
    let mut events = Vec::new();
    let mut source_hd = Vec::new();
    let mut context = HdReceiverContext::new(&pool);
    let mut context_facts = Vec::new();
    let mut owned_packets: Vec<Bytes> = Vec::new();
    let mut rejections = Vec::new();
    let mut decisions = Vec::new();
    let mut source_complete = true;
    let mut native_input_refusal = false;
    let mut serialized_pixels = 0_usize;
    for packet in 0..packets {
        let known = input.take(1)?[0];
        if known > 1 {
            return Err("known flag domain".into());
        }
        let pts = input.i64()?;
        let length = usize::try_from(input.u32()?).map_err(|_| "packet size")?;
        let bytes = input.take(length)?;
        source.push(format!(
            "{{\"packet\":{packet},\"known\":{},\"pts_raw\":{pts},\"payload_hex\":\"{}\"}}",
            known != 0,
            hex(bytes)
        ));
        let packet_owner = Bytes::copy_from_slice(&pool, bytes).map_err(|e| format!("{e:?}"))?;
        owned_packets.push(packet_owner.clone());
        let mut job = HdContextJob::new(packet_owner, &pool, HdLimits::default(), &context)
            .map_err(|e| format!("{e:?}"))?;
        // These original controls end this supplied input; no fragment-codec claim.
        job.finish_input();
        loop {
            match job
                .poll(&mut WorkBudget::new(quantum), &cancel)
                .map_err(|e| format!("{e:?}"))?
            {
                HdReceiverStatus::Ready => break,
                HdReceiverStatus::Yield => (),
                HdReceiverStatus::NeedInputEnd => {
                    return Err("explicit source end not applied".into());
                }
            }
        }
        let output = job.try_finish(&cancel).map_err(|e| format!("{e:?}"))?;
        let (output, picture_state, next_state) = output.into_parts();
        context_facts.push(format!(
            "{{\"packet\":{packet},\"parsed_source_facts\":{},\"picture\":{},\"after\":{}}}",
            facts_json(output.parsed_context_facts(), output.packet(), None),
            context_json(&picture_state, &owned_packets)?,
            context_json(&next_state, &owned_packets)?
        ));
        context = next_state;
        let facts = output.source_facts();
        source_hd.push(format!(
            "{{\"packet\":{packet},\"facts\":{}}}",
            facts_json(
                facts,
                output.packet(),
                output.picture().map(|p| p.bitmap().pixels().as_ref())
            )
        ));
        if let Some(error) = output.source_error() {
            source_complete = false;
            native_input_refusal |= bytes.is_empty();
            rejections.push(format!(
                "{{\"packet\":{packet},\"source_error\":{}}}",
                quoted(&format!("{error:?}"))
            ));
            decisions.push(format!(
                "{{\"packet\":{packet},\"disposition\":\"source-rejected-at-explicit-input-end\"}}"
            ));
            continue;
        }
        if facts.last_opaque_tail.is_some() || facts.stopped_at_link.is_some() {
            source_complete = false;
        }
        let (_raw, picture, _error, _facts) = output.into_parts();
        let picture = picture.ok_or("receiver picture absent")?;
        let clock = microseconds(known != 0, pts, tb);
        let (event, count) = if facts.eight_bit {
            if picture.ycrcb().is_none() {
                decisions.push(format!("{{\"packet\":{packet},\"disposition\":\"missing-source-YCrCb-native-noevent\"}}"));
                continue;
            }
            let mut palette = picture_state
                .palette_job(&picture)
                .map_err(|e| format!("{e:?}"))?;
            while palette
                .poll(&mut WorkBudget::new(quantum), &cancel)
                .map_err(|e| format!("{e:?}"))?
                != HdStatus::Ready
            {}
            let palette = palette.try_finish(&cancel).map_err(|e| format!("{e:?}"))?;
            let (_raw, bitmap, _facts) = picture.into_parts();
            render(
                bitmap, palette, facts, &pool, quantum, &cancel, packet, length, clock, canvas,
            )?
        } else {
            let palette = picture_state
                .four_palette(config.palette.as_ref())
                .map_err(|e| format!("{e:?}"))?;
            let (_raw, bitmap, _facts) = picture.into_parts();
            render(
                bitmap,
                palette.argb,
                facts,
                &pool,
                quantum,
                &cancel,
                packet,
                length,
                clock,
                canvas,
            )?
        };
        serialized_pixels = serialized_pixels
            .checked_add(count)
            .ok_or("observer pixel overflow")?;
        if serialized_pixels > 8 * 1024 * 1024 {
            return Err("observer pixel limit".into());
        }
        decisions.push(format!(
            "{{\"packet\":{packet},\"disposition\":{}}}",
            quoted(if event.is_some() {
                "rendered"
            } else {
                "explicit-receiver-omission"
            })
        ));
        if let Some(event) = event {
            events.push(event);
        }
    }
    if input.at != raw.len() {
        return Err("trailing protocol bytes".into());
    }
    Ok(format!(
        "{{\"private_hex\":\"{}\",\"timebase\":[{},{}],\"source_packets\":[{}],\"extended_source\":[{}],\"events\":[{}],\"errors\":[],\"source_rejections\":[{}],\"source_program_complete\":{},\"modeled_C2_empty_input_refusal\":{},\"receiver_decisions\":[{}],\"receiver_context\":[{}],\"complete_packet_context_scope\":true,\"full_decoder_qualification\":false}}",
        hex(private),
        numerator,
        denominator,
        source.join(","),
        source_hd.join(","),
        events.join(","),
        rejections.join(","),
        source_complete,
        native_input_refusal,
        decisions.join(","),
        context_facts.join(",")
    ))
}
fn main() {
    let outcome = (|| {
        let mut args = std::env::args().skip(1);
        let path = args.next().ok_or("input path required")?;
        let quantum: u64 = args
            .next()
            .ok_or("work quantum required")?
            .parse()
            .map_err(|_| "work quantum")?;
        if args.next().is_some() {
            return Err("unexpected argument".into());
        }
        let file = std::fs::File::open(path).map_err(|e| format!("{e}"))?;
        let mut bytes = Vec::new();
        file.take((MAX_INPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{e}"))?;
        if bytes.len() > MAX_INPUT {
            return Err("input byte limit".into());
        }
        run(&bytes, quantum)
    })();
    match outcome {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}
