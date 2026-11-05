#![forbid(unsafe_code)]
//! Private source-only correctness observer. Never reads native expectations.
use std::{fmt::Write as _, io::Read, sync::atomic::AtomicBool};
use triergon_core::{Budget, Bytes, Rounding, Time, TimeBase, WorkBudget};
use triergon_subtitles::dvd::{
    ConfigJob, ConfigLimits, ConfigStatus, HdFacts, HdLimits, HdSpan, HdSpuJob, HdStatus,
    NativeCropMode, NativeProjectionJob, ProjectionLimits, ProjectionStatus,
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
fn facts_json(f: HdFacts, raw: &[u8], indices: &[u8]) -> String {
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
        "{{\"ycrcb\":{},\"contrast\":{},\"area\":{},\"area_wire\":{},\"fields\":{},\"fields_wire\":{},\"eight_bit\":{},\"forced\":{},\"menu\":{},\"start_date_raw\":{},\"stop_date_raw\":{},\"source_rgb\":null,\"source_indices_hex\":\"{}\"}}",
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
        hex(indices)
    )
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
    let mut errors = Vec::new();
    let mut source_hd = Vec::new();
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
        let decoded = (|| {
            let owned = Bytes::copy_from_slice(&pool, bytes)?;
            let mut job = HdSpuJob::new(owned, &pool, HdLimits::default())?;
            while job.poll(&mut WorkBudget::new(quantum), &cancel)? != HdStatus::Ready {}
            job.try_finish(&cancel)
        })();
        let picture = match decoded {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!(
                    "{{\"packet\":{packet},\"error\":{}}}",
                    quoted(&format!("{e:?}"))
                ));
                continue;
            }
        };
        let facts = picture.facts();
        source_hd.push(format!(
            "{{\"packet\":{packet},\"facts\":{}}}",
            facts_json(facts, picture.packet(), picture.bitmap().pixels())
        ));
        let palette_result = (|| {
            let mut job = picture.palette_job()?;
            while job.poll(&mut WorkBudget::new(quantum), &cancel)? != HdStatus::Ready {}
            job.try_finish(&cancel)
        })();
        let palette = match palette_result {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!(
                    "{{\"packet\":{packet},\"error\":{}}}",
                    quoted(&format!("{e:?}"))
                ));
                continue;
            }
        };
        let (_raw, bitmap, _facts) = picture.into_parts();
        let mut projection = NativeProjectionJob::new(
            bitmap,
            palette,
            if facts.menu {
                NativeCropMode::Menu
            } else {
                NativeCropMode::Ordinary
            },
            &pool,
            ProjectionLimits::default(),
        )
        .map_err(|e| format!("{e:?}"))?;
        while projection
            .poll(&mut WorkBudget::new(quantum), &cancel)
            .map_err(|e| format!("{e:?}"))?
            != ProjectionStatus::Ready
        {}
        let projection = projection
            .try_finish(&cancel)
            .map_err(|e| format!("{e:?}"))?;
        let Some(view) = projection.native_view() else {
            continue;
        };
        let crop = view.crop();
        serialized_pixels = serialized_pixels
            .checked_add(usize::from(crop.width) * usize::from(crop.height))
            .ok_or("pixel overflow")?;
        if serialized_pixels > 8 * 1024 * 1024 {
            return Err("observer pixel limit".into());
        }
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
        events.push(format!(concat!("{{\"kind\":\"subtitle\",\"packet\":{},\"packet_offset\":0,\"consumed\":{},\"flush\":false,\"format\":0,",
   "\"pts_microseconds\":{},\"start_display_ms\":{},\"end_display_ms\":{},\"context_canvas\":[{},{}],",
   "\"rectangles\":[{{\"order\":0,\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"flags\":{},",
   "\"native_stride\":{},\"indices_hex\":\"{}\",\"palette_entries\":256,\"raw_native_palette_hex\":\"{}\",\"palette_argb_u32\":[{}]}}]}}"),
   packet,length,optional(microseconds(known!=0,pts,tb)),start,stop,canvas[0],canvas[1],crop.area.left,crop.area.top,crop.width,crop.height,u8::from(facts.forced),crop.width,pixels,hex(&raw_palette),words));
    }
    if input.at != raw.len() {
        return Err("trailing protocol bytes".into());
    }
    Ok(format!(
        "{{\"private_hex\":\"{}\",\"timebase\":[{},{}],\"source_packets\":[{}],\"extended_source\":[{}],\"events\":[{}],\"errors\":[{}],\"full_decoder_qualification\":false}}",
        hex(private),
        numerator,
        denominator,
        source.join(","),
        source_hd.join(","),
        events.join(","),
        errors.join(",")
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
