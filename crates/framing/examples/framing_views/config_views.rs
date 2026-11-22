use std::{ffi::OsString, sync::atomic::AtomicBool};

use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    BuildStep,
    aac::{
        AacAscScopeParser, AacAudioSpecificConfig, AacConfigLimits, AacDecoderConfigDecision,
        AacDecoderConfigPolicy, AacSource, decoder_channel_view, decoder_configuration_view,
    },
    hevc::inspect_config_prefix,
    vp9::{Vp9ConfigContext, Vp9ConfigLimits, Vp9ConfigParser},
};

use super::{
    obu::record,
    protocol::{Action, Operation},
};

#[path = "config_views/eac3.rs"]
mod eac3;

static CANCEL: AtomicBool = AtomicBool::new(false);

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("configuration argument is not UTF-8"))
        .collect::<Result<Vec<_>, _>>()?;
    let (name, run): (&str, Action<'a>) = match (prefix, args.as_slice()) {
        ("hvcc-prefix-", []) => ("hevc-hvcc-prefix", hvcc(input, false)),
        ("vpcc-", []) => ("vp9-config", vpcc(input, false, None)),
        ("vpcc-", [flags]) => (
            "vp9-config",
            vpcc(
                input,
                false,
                Some(flags.parse().map_err(|_| "invalid FullBox flags")?),
            ),
        ),
        ("eac3-bsi-", [bits, bytes]) => (
            "eac3-bsi",
            eac3::inspect(input, number(bits)?, number(bytes)?),
        ),
        ("config-", ["vpcc"]) => ("codec-config-views", vpcc(input, true, None)),
        ("config-", ["hvcc-written"]) => ("codec-config-views", hvcc(input, true)),
        ("config-", ["asc", bits]) => (
            "codec-config-views",
            asc(input, number(bits)?, false, false),
        ),
        ("config-", ["asc-truncated", bits]) => {
            ("codec-config-views", asc(input, number(bits)?, false, true))
        }
        ("config-", ["asc-mov", bits]) => {
            ("codec-config-views", asc(input, number(bits)?, true, false))
        }
        ("config-", ["asc-frame", config]) => {
            let config = Some(*config)
                .filter(|value| {
                    value.len().is_multiple_of(2) && value.bytes().all(|b| b.is_ascii_hexdigit())
                })
                .map(triergon_harness_protocol::unhex)
                .ok_or("invalid ASC for raw access unit")?;
            ("codec-config-views", asc_frame(config))
        }
        ("config-", ["eac3"]) => ("codec-config-views", eac3::decoded(input)),
        _ => return Err(format!("invalid {prefix} configuration arguments")),
    };
    Ok(Operation { name, run })
}

fn number(number: &str) -> Result<usize, String> {
    number
        .parse()
        .map_err(|_| format!("invalid limit {number}"))
}

fn error(value: Error) -> String {
    format!("{value:?}")
}

fn refusal(value: Error) -> Result<(), String> {
    match value {
        Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated | Error::Limit { .. } => {
            Ok(())
        }
        other => Err(error(other)),
    }
}

fn hvcc(input: &[u8], movie: bool) -> Action<'_> {
    Box::new(move |work, mut trace| {
        let raw = if movie { hvcc_box(input)? } else { input };
        let fields = match inspect_config_prefix(raw) {
            Ok(fields) => fields,
            Err(Error::Truncated) => return record(work, &mut trace, 8, &[], &[]),
            Err(other) => return Err(error(other)),
        };
        if movie {
            return record(
                work,
                &mut trace,
                3,
                &[
                    fields.profile_idc().into(),
                    fields.level_idc().into(),
                    fields.chroma_format_idc().into(),
                    fields.bit_depth_luma_minus8().into(),
                    fields.bit_depth_chroma_minus8().into(),
                ],
                &[],
            );
        }
        for (index, value) in [
            fields.version().into(),
            fields.profile_space().into(),
            fields.tier_flag().into(),
            fields.profile_idc().into(),
            fields.profile_compatibility_flags().into(),
            fields.constraint_indicator_flags(),
            fields.level_idc().into(),
            fields.min_spatial_segmentation_idc().into(),
            fields.parallelism_type().into(),
            fields.chroma_format_idc().into(),
            fields.bit_depth_luma_minus8().into(),
            fields.bit_depth_chroma_minus8().into(),
            fields.average_frame_rate().into(),
            fields.constant_frame_rate().into(),
            fields.num_temporal_layers().into(),
            fields.temporal_id_nested().into(),
            fields.length_size_minus_one().into(),
            fields.array_count().into(),
            fields.syntax_issues().into(),
        ]
        .into_iter()
        .enumerate()
        {
            record(work, &mut trace, 3, &[index as u64, value], &[])?;
        }
        Ok(())
    })
}

// The authored movies carry one ordinary, unextended visual sample entry.
fn hvcc_box(mut scope: &[u8]) -> Result<&[u8], String> {
    for (name, skip) in [
        (b"moov", 0),
        (b"trak", 0),
        (b"mdia", 0),
        (b"minf", 0),
        (b"stbl", 0),
        (b"stsd", 8),
        (b"hev1", 78),
        (b"hvcC", 0),
    ] {
        let mut present = false;
        while let Some(header) = scope.get(..8) {
            let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
            let body = scope
                .get(8..length)
                .ok_or("invalid box extent in authored HEVC movie")?;
            if &header[4..] == name {
                scope = body.get(skip..).ok_or("short HEVC sample entry")?;
                present = true;
                break;
            }
            scope = &scope[length..];
        }
        if !present {
            return Err(format!(
                "HEVC movie is missing {}",
                String::from_utf8_lossy(name)
            ));
        }
    }
    Ok(scope)
}

fn vpcc(input: &[u8], reference: bool, flags: Option<u32>) -> Action<'_> {
    Box::new(move |work, mut trace| {
        let Some((header, body)) = input.split_first_chunk::<4>() else {
            return record(work, &mut trace, 8, &[], &[]);
        };
        let pool = Budget::new(input.len() + 4096).map_err(error)?;
        let context = Vp9ConfigContext {
            version: header[0],
            flags: flags
                .unwrap_or_else(|| u32::from_be_bytes([0, header[1], header[2], header[3]])),
        };
        let result = Vp9ConfigParser::new(
            &pool,
            SourceId(1),
            Bytes::copy_from_slice(&pool, body).map_err(error)?,
            context,
            Vp9ConfigLimits {
                maximum_record_bytes: 65_543,
                maximum_input_window: 4096,
            },
        )
        .and_then(|mut parser| parser.step(&mut WorkBudget::new(32), &CANCEL));
        let config = match result {
            Ok(BuildStep::Ready(config)) => config,
            Ok(_) => return Err("VP9 configuration did not complete".into()),
            Err(value) => {
                refusal(value)?;
                return record(work, &mut trace, 8, &[], &[]);
            }
        };
        let fields = config.fields();
        let (primaries, transfer, matrix) = (
            fields.colour_primaries(),
            fields.transfer_characteristics(),
            fields.matrix_coefficients(),
        );
        if reference {
            // The live projection asks whether a VP9 track can publish this
            // record. The wire oracle also keeps the diagnostic and the bytes.
            if fields.initialization_size() != 0 {
                return record(work, &mut trace, 8, &[], &[]);
            }
            // The MOV demuxer preserves reserved values 0 and 3 in range.
            // Unmapped values appear as AVCOL_*_UNSPECIFIED in codecpar.
            let primaries = if matches!(primaries, 0..=12 | 22) {
                primaries
            } else {
                2
            };
            let transfer = if transfer <= 18 { transfer } else { 2 };
            let matrix = if matrix <= 17 { matrix } else { 2 };
            return record(
                work,
                &mut trace,
                3,
                &[
                    1 + u64::from(fields.video_full_range()),
                    primaries.into(),
                    transfer.into(),
                    matrix.into(),
                ],
                &[],
            );
        }
        for (index, value) in [
            fields.profile().into(),
            fields.level().into(),
            fields.bit_depth().into(),
            fields.chroma_subsampling().into(),
            fields.video_full_range().into(),
            primaries.into(),
            transfer.into(),
            matrix.into(),
            fields.initialization_size().into(),
            config.syntax_issues().bits().into(),
        ]
        .into_iter()
        .enumerate()
        {
            record(work, &mut trace, 3, &[index as u64, value], &[])?;
        }
        let bytes = config.initialization_bytes().map_err(error)?;
        record(work, &mut trace, 2, &[bytes.len() as u64], bytes.as_ref())
    })
}

fn asc_scopes(
    pool: &Budget,
    input: &[u8],
    bits: usize,
) -> Result<triergon_framing::aac::AacAscScopeViews, Error> {
    let source = AacSource {
        source: SourceId(1),
        generation: 0,
        offset: 0,
    };
    let limits = AacConfigLimits {
        maximum_record_bytes: 4096,
        maximum_pce_elements: 70,
        maximum_comment_bytes: 255,
    };
    let raw = Bytes::copy_from_slice(pool, input)?;
    let mut parser = AacAscScopeParser::new(pool, source, raw, bits, limits)?;
    match parser.step(&mut WorkBudget::new(32768), &CANCEL)? {
        BuildStep::Ready(views) => Ok(views),
        _ => Err(Error::Invalid("ASC parser exhausted the worker grant")),
    }
}

fn decoder_channels(config: &AacAudioSpecificConfig) -> Result<Option<u16>, String> {
    let view = decoder_channel_view(config.facts());
    let policy = AacDecoderConfigPolicy {
        allow_explicit_core_frequency: false,
        maximum_channels: 64,
    };
    match decoder_configuration_view(config, policy).map_err(error)? {
        AacDecoderConfigDecision::Eligible(channel_view) if channel_view == view => {
            Ok(Some(view.channels))
        }
        AacDecoderConfigDecision::Eligible(_) => Err("ASC channel views disagree".into()),
        AacDecoderConfigDecision::Refused(_) => Ok(None),
    }
}

fn asc(input: &[u8], bits: usize, movie: bool, compare_padded: bool) -> Action<'_> {
    Box::new(move |work, mut trace| {
        let pool = Budget::new(65536).map_err(error)?;
        let physical = input.len() * 8;
        let declared = if bits > physical {
            // Its constructor must refuse a declaration beyond the physical source.
            match asc_scopes(&pool, input, bits) {
                Err(Error::Invalid(_)) => {}
                Err(other) => return Err(error(other)),
                Ok(_) => return Err("ASC accepted an overlong declaration".into()),
            }
            None
        } else {
            Some(bits)
        };
        if movie {
            let Some(size) = declared.filter(|size| size.is_multiple_of(8)) else {
                return record(work, &mut trace, 8, &[], &[]);
            };
            let views = asc_scopes(&pool, input, size).map_err(error)?;
            let Some(config) = views.declared().parsed() else {
                return record(work, &mut trace, 8, &[], &[]);
            };
            let bytes = &views.source_bytes()[..views.declared_bits() / 8];
            record(work, &mut trace, 2, &[bytes.len() as u64], bytes)?;
            let facts = config.facts();
            return record(
                work,
                &mut trace,
                3,
                &[
                    facts.signalled_render_rate_hz().into(),
                    facts.core_channels.into(),
                    facts.core_samples_per_frame().into(),
                ],
                &[],
            );
        }
        let views = match asc_scopes(&pool, input, declared.unwrap_or(physical)) {
            Ok(views) => Some(views),
            Err(value) => {
                refusal(value)?;
                None
            }
        };
        if let Some(views) = &views
            && (views.declared_bits() != declared.unwrap_or(physical)
                || views.physical_bits() != physical
                || views.same_extent() != (views.declared_bits() == physical))
        {
            return Err("ASC scope geometry disagrees with the source extent".into());
        }
        record(
            work,
            &mut trace,
            2,
            &[bits as u64, physical as u64, u64::from(bits == physical)],
            &[],
        )?;
        let outcomes = views
            .as_ref()
            .map(|views| [views.declared(), views.physical()]);
        let padded = if compare_padded {
            // The authored cut ends inside a required syntax element. Verify
            // the exact cutoff before comparing FFmpeg's byte-sized input.
            if bits == 0
                || bits.is_multiple_of(8)
                || outcomes.and_then(|outcomes| outcomes[0].error()) != Some(Error::Truncated)
            {
                return Err("ASC bit cut should truncate the authored syntax".into());
            }
            let mut masked = input[..bits.div_ceil(8)].to_vec();
            *masked.last_mut().ok_or("missing ASC padding byte")? &= u8::MAX << (8 - bits % 8);
            Some(asc_scopes(&pool, &masked, masked.len() * 8).map_err(error)?)
        } else {
            None
        };
        for scope in 0..2 {
            let outcome = if scope == 0
                && let Some(views) = &padded
            {
                Some(views.declared())
            } else {
                outcomes
                    .filter(|_| scope != 0 || declared.is_some())
                    .map(|outcomes| outcomes[scope])
            };
            let config = outcome.and_then(triergon_framing::aac::AacAscScopeOutcome::parsed);
            if let Some(config) = config
                && let Some(channels) = decoder_channels(config)?
            {
                record(work, &mut trace, 3, &[scope as u64, channels.into()], &[])?;
                if let Some(out) = trace.as_mut() {
                    writeln!(
                        out,
                        "# ASC rate={} reason={:?}",
                        config.facts().signalled_render_rate_hz(),
                        decoder_channel_view(config.facts()).reason
                    )
                    .map_err(|value| value.to_string())?;
                }
                continue;
            }
            record(work, &mut trace, 8, &[scope as u64], &[])?;
        }
        Ok(())
    })
}

fn asc_frame(raw: Vec<u8>) -> Action<'static> {
    Box::new(move |work, mut trace| {
        let pool = Budget::new(65536).map_err(error)?;
        let views = match asc_scopes(&pool, &raw, raw.len() * 8) {
            Ok(views) => views,
            Err(value) => {
                refusal(value)?;
                return record(work, &mut trace, 8, &[], &[]);
            }
        };
        let Some(config) = views.physical().parsed() else {
            return record(work, &mut trace, 8, &[], &[]);
        };
        let Some(channels) = decoder_channels(config)? else {
            return record(work, &mut trace, 8, &[], &[]);
        };
        let facts = config.facts();
        let rate = facts.signalled_render_rate_hz();
        let samples = u32::from(facts.core_samples_per_frame()) * rate / facts.core_frequency.hz;
        record(
            work,
            &mut trace,
            3,
            &[rate.into(), channels.into(), samples.into()],
            &[],
        )
    })
}
