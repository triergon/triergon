use std::ffi::OsString;
use std::sync::atomic::AtomicBool;

use triergon_core::WorkBudget;
use triergon_matroska::{Configuration, Error, Limits, ParseStep, Presence, TrackExtensions};

use super::protocol::Operation;
use super::side_data::{Input, Rows};

const DVCC: u64 = 0x64766343;
const DVVC: u64 = 0x64767643;
const HVCE: u64 = 0x68766345;

pub(super) fn track_extensions(
    configuration: &Configuration,
    track: usize,
    cancel: &AtomicBool,
) -> triergon_matroska::Result<TrackExtensions> {
    let mut job = configuration.track_extensions_job(track, Limits::default())?;
    match job.step(&mut WorkBudget::new(u64::MAX), cancel)? {
        ParseStep::Event(extensions) => Ok(extensions),
        _ => Err(Error::Invalid("track extensions did not finish")),
    }
}

fn presence(presence: Presence) -> u64 {
    match presence {
        Presence::Absent => 0,
        Presence::Empty => 1,
        Presence::Value => 2,
    }
}

fn source(extensions: &TrackExtensions, rows: &mut Rows<'_, '_>) -> triergon_matroska::Result<()> {
    let raw = extensions.raw_track_entry();
    rows.record(
        2,
        &[
            0,
            presence(extensions.maximum_presence()),
            extensions.max_block_addition_id(),
            raw.len() as u64,
        ],
        raw,
    )?;
    for mapping in extensions.mappings() {
        rows.record(
            2,
            &[
                1,
                presence(mapping.value_presence()),
                mapping.value().unwrap_or_default(),
                presence(mapping.kind_presence()),
                mapping.kind(),
                u64::from(mapping.type_zero_value_conflict()),
                mapping.raw().len() as u64,
            ],
            mapping.raw(),
        )?;
        rows.optional(2, 2, mapping.name()?.map(str::as_bytes))?;
        rows.optional(2, 3, mapping.raw_name())?;
        rows.optional(2, 4, mapping.extra_data())?;
    }
    for unknown in extensions.unknown_mapping_fields() {
        rows.record(
            2,
            &[
                5,
                unknown.mapping_index() as u64,
                unknown.id(),
                unknown.raw().len() as u64,
            ],
            unknown.raw(),
        )?;
        rows.record(2, &[6, unknown.data().len() as u64], unknown.data())?;
    }
    Ok(())
}

fn dolby_configuration(data: &[u8]) -> triergon_matroska::Result<[u64; 9]> {
    // Dolby Vision ISOBMFF v2.1.2, DOVIDecoderConfigurationRecord.
    let &[major, minor, profile_level, level_flags, ref tail @ ..] = data else {
        return Err(Error::Truncated);
    };
    let compatibility = tail.first().copied().unwrap_or_default();
    Ok([
        major,
        minor,
        profile_level >> 1,
        ((profile_level & 1) << 5) | (level_flags >> 3),
        (level_flags >> 2) & 1,
        (level_flags >> 1) & 1,
        level_flags & 1,
        compatibility >> 4,
        (compatibility >> 2) & 3,
    ]
    .map(u64::from))
}

fn published(
    extensions: &TrackExtensions,
    rows: &mut Rows<'_, '_>,
) -> triergon_matroska::Result<()> {
    let (mut dolby, mut hevc) = (None, None);
    for mapping in extensions.mappings() {
        let Some(data) = mapping.extra_data() else {
            continue;
        };
        match mapping.kind() {
            DVCC | DVVC => dolby = Some(dolby_configuration(data)?),
            HVCE => hevc = Some(data),
            _ => (),
        }
    }
    if let Some(fields) = dolby {
        rows.record(2, &[&[1], &fields[..]].concat(), &[])?;
    }
    rows.optional(2, 2, hevc)
}

fn run(bytes: &[u8], raw: bool, rows: &mut Rows<'_, '_>) -> triergon_matroska::Result<()> {
    let mut input = Input::new(bytes, false)?;
    input.open()?;
    let configuration = input
        .reader
        .configuration()
        .ok_or(Error::Invalid("missing configuration"))?;
    for track in 0..configuration.tracks().len() {
        let extensions = track_extensions(configuration, track, &input.cancel)?;
        rows.record(1, &[track as u64], &[])?;
        if raw {
            source(&extensions, rows)?;
        } else {
            published(&extensions, rows)?;
        }
    }
    Ok(())
}

pub(super) fn operation<'a>(
    _prefix: &str,
    bytes: &'a [u8],
    args: &[OsString],
) -> Result<Operation<'a>, String> {
    let raw = match args {
        [] => false,
        [mode] if mode == "source" => true,
        _ => return Err("expected no argument or source".into()),
    };
    Ok(Operation {
        name: "mkv-track-extensions",
        run: Box::new(move |work, trace| {
            let mut rows = Rows { work, trace };
            let result = run(bytes, raw, &mut rows);
            rows.finish(result)
        }),
    })
}
