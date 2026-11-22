use std::collections::BTreeSet;
use std::ffi::OsString;
use std::sync::atomic::AtomicBool;

use triergon_core::{Budget, Bytes, WorkBudget};
use triergon_subtitles::text::tx3g_config::{
    ConfigJob, ConfigLayout, ConfigLimits, ConfigStatus, Configuration,
};

use super::protocol::Operation;
use triergon_subtitles::text::Format;
use triergon_subtitles::text::tx3g_semantics::{
    PaintIntent, PointQuery, SemanticJob, SemanticLimits, SemanticStatus, Semantics, StyleOrigin,
    Truth, ValueOrigin, WrapMode,
};

use super::text::{Records, packet_input};

const SLICE: u64 = 4096;

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    if prefix == "tx3g-semantics-" {
        return semantic_operation(input, arguments);
    }
    let [mode] = arguments else {
        return Err(format!(
            "usage: {prefix} defaults|sample-entry|unicode|ass-header"
        ));
    };
    let (layout, unicode, ass) = match mode.to_str() {
        Some("defaults") => (ConfigLayout::DefaultsRecord, false, false),
        Some("sample-entry") => (ConfigLayout::SampleEntryBody, false, false),
        Some("unicode") => (ConfigLayout::DefaultsRecord, true, false),
        Some("ass-header") => (ConfigLayout::DefaultsRecord, true, true),
        _ => return Err(format!("invalid tx3g view: {}", mode.to_string_lossy())),
    };
    Ok(Operation {
        name: "tx3g-config",
        run: Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            match configuration(input, layout, unicode) {
                Ok(config) if ass => ass_header(&mut records, &config),
                Ok(config) => fields(&mut records, &config),
                Err(_) => records.refused(),
            }
        }),
    })
}

fn configuration(
    input: &[u8],
    layout: ConfigLayout,
    unicode: bool,
) -> triergon_core::Result<Configuration> {
    let pool = Budget::new(16 * 1024 * 1024)?;
    let bytes = Bytes::copy_from_slice(&pool, input)?;
    let limits = ConfigLimits::default();
    let mut job = if unicode {
        ConfigJob::new_unicode_fonts(&bytes, layout, &pool, limits)?
    } else {
        ConfigJob::new(&bytes, layout, &pool, limits)?
    };
    let cancel = AtomicBool::new(false);
    while job.poll(&mut WorkBudget::new(SLICE), &cancel)? == ConfigStatus::Pending {}
    job.take_output(&cancel)
}

fn fields(records: &mut Records<'_, '_>, config: &Configuration) -> Result<(), String> {
    let defaults = config.defaults;
    let style = defaults.style;
    let mut words = vec![0, u64::from(defaults.data_reference_index.is_some())];
    if let Some(index) = defaults.data_reference_index {
        words.push(index.into());
    }
    words.extend([
        u64::from(defaults.display_flags),
        defaults.horizontal as u64,
        defaults.vertical as u64,
    ]);
    words.extend(defaults.background.map(u64::from));
    words.extend(defaults.text_box.map(|value| value as u64));
    words.extend([
        u64::from(style.start),
        u64::from(style.end),
        u64::from(style.font_id),
        u64::from(style.face_flags),
        u64::from(style.font_size),
    ]);
    words.extend(style.rgba.map(u64::from));
    records.row(3, &words, &[])?;
    for font in config.fonts.iter() {
        records.row(
            3,
            &[
                1,
                font.id.into(),
                font.name_range.start as u64,
                font.name_range.end as u64,
            ],
            &config.raw.as_ref()[font.name_range.clone()],
        )?;
    }
    for item in config.boxes.iter() {
        records.row(
            3,
            &[
                2,
                u32::from_be_bytes(item.kind).into(),
                item.range.start as u64,
                item.range.end as u64,
            ],
            &config.raw.as_ref()[item.range.clone()],
        )?;
    }
    records.row(7, &[], &[])
}

fn ass_header(records: &mut Records<'_, '_>, config: &Configuration) -> Result<(), String> {
    let defaults = config.defaults;
    let style = defaults.style;
    let raw_font = config
        .fonts
        .iter()
        .rfind(|font| font.id == style.font_id)
        .map_or(&[][..], |font| {
            &config.raw.as_ref()[font.name_range.clone()]
        });
    // The ASS decoder's header exposes one C string. The configuration
    // oracle compares every font record and its full length-delimited name.
    let decoded;
    let font = if let Some(bytes) = raw_font.strip_prefix(&[0xfe, 0xff]) {
        decoded = char::decode_utf16(
            bytes
                .chunks_exact(2)
                .map(|word| u16::from_be_bytes([word[0], word[1]])),
        )
        .collect::<Result<String, _>>()
        .map_err(|error| error.to_string())?;
        decoded.as_bytes()
    } else {
        raw_font
    };
    let font = font.split(|&byte| byte == 0).next().unwrap_or_default();
    let [r, g, b, a] = style.rgba;
    let primary = u32::from_be_bytes([255 - a, b, g, r]);
    let [r, g, b, a] = defaults.background;
    let background = u32::from_be_bytes([255 - a, b, g, r]);
    let column = match defaults.horizontal {
        -1 => 3,
        1 => 2,
        _ => 1,
    };
    let row = match defaults.vertical {
        -1 => 0,
        1 => 3,
        _ => 6,
    };
    records.row(
        3,
        &[
            style.font_size.into(),
            primary.into(),
            background.into(),
            u64::from(style.face_flags & 1 != 0),
            u64::from(style.face_flags & 2 != 0),
            u64::from(style.face_flags & 4 != 0),
            row + column,
            font.len() as u64,
        ],
        font,
    )
}

fn semantic_operation<'a>(
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [timescale] = arguments else {
        return Err("usage: tx3g-semantics TIMESCALE|-".into());
    };
    let scale = if timescale == "-" {
        None
    } else {
        Some(
            timescale
                .to_str()
                .ok_or("invalid timescale")?
                .parse::<u32>()
                .map_err(|error| error.to_string())?,
        )
    };
    let (head, payload) = input.split_at_checked(4).ok_or("short semantic envelope")?;
    let length = u32::from_be_bytes(head.try_into().map_err(|_| "bad semantic envelope")?);
    let (description, sample) = if length == u32::MAX {
        (None, payload)
    } else {
        let (description, sample) = payload
            .split_at_checked(length as usize)
            .ok_or("short semantic description")?;
        (Some(description), sample)
    };
    Ok(Operation {
        name: "tx3g-semantics",
        run: Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            let Ok(value) = semantics(sample, description, scale) else {
                return records.refused();
            };
            semantic_rows(&mut records, &value)
        }),
    })
}

fn semantics(
    sample: &[u8],
    description: Option<&[u8]>,
    timescale: Option<u32>,
) -> triergon_core::Result<Semantics> {
    let pool = Budget::new(32 * 1024 * 1024)?;
    let mut input = packet_input(Bytes::copy_from_slice(&pool, sample)?, Format::Tx3g);
    input.codec_private = description
        .map(|data| Bytes::copy_from_slice(&pool, data))
        .transpose()?;
    let mut job = SemanticJob::new(
        &input,
        description.map(|_| ConfigLayout::DefaultsRecord),
        timescale,
        &pool,
        SemanticLimits::default(),
        0,
    )?;
    let cancel = AtomicBool::new(false);
    loop {
        match job.poll(&mut WorkBudget::new(SLICE), &cancel)? {
            SemanticStatus::Pending => {}
            SemanticStatus::Ready => return job.take_output(&cancel),
            SemanticStatus::NeedOutput => {
                return Err(triergon_core::Error::Invalid("semantic output budget"));
            }
        }
    }
}

fn origin(origin: ValueOrigin) -> u64 {
    match origin {
        ValueOrigin::Sample => 1,
        ValueOrigin::Description => 2,
        ValueOrigin::SpecifiedAbsentDefault => 3,
    }
}

fn semantic_rows(records: &mut Records<'_, '_>, value: &Semantics) -> Result<(), String> {
    let count = value.character_count();
    let text = value.source().decoded_utf8.as_deref().unwrap_or_default();
    records.row(3, &[0, count as u64, text.len() as u64], text)?;
    let globals = value.globals();
    let delay = globals.scroll_delay_ticks();
    let validation = value.validation_context();
    let mut header = vec![
        1,
        u64::from(delay.value),
        origin(delay.origin),
        globals.wrap.as_ref().map_or(0, |wrap| match wrap.value {
            WrapMode::NoWrap => 1,
            WrapMode::Soft => 2,
        }),
        globals.display.map_or(0, |_| 1),
        globals
            .display
            .map_or(0, |flags| u64::from(flags.raw_flags)),
        validation.description_parsed.into(),
        validation.conditional_combinations_checked.into(),
    ];
    if let Some(box_) = globals.resolved_text_box {
        header.push(origin(box_.origin));
        header.extend(box_.value.map(|v| v as u64));
    } else {
        header.push(0);
    }
    if let Some(disparity) = globals.resolved_disparity {
        header.extend([origin(disparity.origin), disparity.value as u64]);
        if let Some((left, right)) = globals.disparity_pixels() {
            header.extend([left as u64, right as u64]);
        }
    } else {
        header.push(0);
    }
    records.row(3, &header, &[])?;
    for font in value.fonts() {
        records.row(
            3,
            &[
                2,
                font.id.into(),
                font.raw_name.start as u64,
                font.raw_name.end as u64,
                font.utf16_bom.into(),
                font.name_utf8.len() as u64,
            ],
            &font.name_utf8,
        )?;
    }
    let raw = value.source().input.payload.as_ref();
    for (index, link) in value.hyperlinks().iter().enumerate() {
        records.row(
            3,
            &[
                3,
                index as u64,
                link.span.characters.start as u64,
                link.span.characters.end as u64,
                link.url.len() as u64,
            ],
            &raw[link.url.clone()],
        )?;
        records.row(
            3,
            &[4, index as u64, link.alternate.len() as u64],
            &raw[link.alternate.clone()],
        )?;
    }
    let mut ticks = BTreeSet::from([None, Some(0)]);
    for event in value.karaoke() {
        ticks.insert(Some(event.start_tick));
        ticks.insert(Some(event.end_tick));
        if let Some(last) = event.end_tick.checked_sub(1) {
            ticks.insert(Some(last));
        }
        records.row(
            3,
            &[
                5,
                event.span.declared_start.into(),
                event.span.declared_end.into(),
                event.span.end_of_text_sentinel.into(),
                event.start_tick.into(),
                event.end_tick.into(),
            ],
            &[],
        )?;
    }
    let cancel = AtomicBool::new(false);
    for character in 0..count {
        for &tick in &ticks {
            let mut query =
                PointQuery::new(value, character, tick).map_err(|error| error.to_string())?;
            let point = loop {
                if let Some(point) = query
                    .poll(&mut WorkBudget::new(SLICE), &cancel)
                    .map_err(|error| error.to_string())?
                {
                    break point;
                }
            };
            let mut words = vec![
                6,
                character as u64,
                tick.is_some().into(),
                u64::from(tick.unwrap_or_default()),
                point.base_style.is_some().into(),
            ];
            if let Some(style) = point.base_style {
                words.extend([
                    u64::from(style.font_id),
                    style.face_flags.into(),
                    style.font_size.into(),
                ]);
                words.extend(style.rgba.map(u64::from));
            }
            match point.style_origin {
                None => words.push(0),
                Some(StyleOrigin::Description) => words.push(1),
                Some(StyleOrigin::Sample(index)) => words.extend([2, index as u64]),
            }
            for index in [point.font_index, point.hyperlink] {
                words.push(index.is_some().into());
                if let Some(index) = index {
                    words.push(index as u64);
                }
            }
            words.extend([
                u64::from(point.blink_requested),
                point.static_highlight.into(),
                match point.karaoke {
                    Truth::No => 0,
                    Truth::Yes => 1,
                    Truth::Unknown => 2,
                },
            ]);
            match point.paint {
                PaintIntent::RequestedStyle => words.push(0),
                PaintIntent::StaticBackground(colour) => {
                    words.push(1);
                    words.extend(colour.map(u64::from));
                }
                PaintIntent::StaticTerminalInvert => words.push(2),
                PaintIntent::KaraokeForeground(colour) => {
                    words.extend([3, colour.is_some().into()]);
                    if let Some(colour) = colour {
                        words.extend(colour.map(u64::from));
                    }
                }
                PaintIntent::HyperlinkTerminalStyle => words.push(4),
                PaintIntent::UnresolvedDynamicExtent => words.push(5),
            }
            records.row(3, &words, &[])?;
        }
    }
    records.row(7, &[], &[])
}
