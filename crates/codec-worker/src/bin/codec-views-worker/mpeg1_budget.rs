//! Graded caller-owned MPEG-1 encoder pool and retry behaviour.

use std::ffi::OsString;
use std::io::Write;

use triergon::Budget;
use triergon_codec_mpeg1::{Encoder, EncoderConfig, Error, Limits};
use triergon_pixel::{CodedPicture, MacroblockGeometry};

use triergon_harness_runner::{Operation, Totals};

pub(super) fn operation<'a>(
    _: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let mut tokens = std::str::from_utf8(input)
        .map_err(explain)?
        .split_whitespace();
    let mut dimension = || {
        tokens
            .next()
            .ok_or("missing dimension")?
            .parse()
            .map_err(|_| "invalid dimension")
    };
    let geometry = MacroblockGeometry::new(dimension()?, dimension()?).map_err(explain)?;
    if tokens.next().is_some() || !arguments.is_empty() {
        return Err("expected only the coded width and height".into());
    }
    let cfg = EncoderConfig {
        geometry,
        bit_rate: 240_000,
        max_rate: 320_001,
        vbv_bits: 32_001,
        gop: 2,
    };
    Ok(Operation {
        name: "mpeg1-encoder-budget",
        run: Box::new(move |work, trace| run(cfg, work, trace)),
    })
}

fn run(
    cfg: EncoderConfig,
    work: &mut Totals,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let tight = Budget::new(1024).map_err(explain)?;
    let idle = tight.used();
    if !matches!(
        Encoder::new_in(cfg, &tight),
        Err(Error::Budget(triergon::Error::Limit { .. }))
    ) || tight.used() != idle
        || !matches!(
            Encoder::with_limits(
                cfg,
                Limits {
                    live_storage_bytes: 1024
                }
            ),
            Err(Error::Budget(triergon::Error::Limit { .. }))
        )
    {
        return Err("insufficient constructor storage changed the error or leaked".into());
    }
    row(work, &mut trace, 1, &[])?;

    let pool = Budget::new(1 << 20).map_err(explain)?;
    let idle = pool.used();
    let mut encoder = Encoder::new_in(cfg, &pool).map_err(explain)?;
    let mut ordinary = Encoder::new(cfg).map_err(explain)?;
    let mut input = CodedPicture::new(cfg.geometry);
    input.pad_edges();
    let mut output = vec![0xa5, 0x5a];
    let mut expected = Vec::new();
    let held = pool.reserve(pool.available()).map_err(explain)?;
    if !matches!(
        encoder.encode(&input, &mut output),
        Err(Error::Budget(triergon::Error::Backpressure { .. }))
    ) || output != [0xa5, 0x5a]
        || encoder.config() != &cfg
    {
        return Err("failed encode changed the output or configuration".into());
    }
    drop(held);
    row(work, &mut trace, 2, &[])?;

    for index in 0..3 {
        let picture = encoder.encode(&input, &mut output).map_err(explain)?;
        let original = ordinary.encode(&input, &mut expected).map_err(explain)?;
        if picture != original
            || output != expected
            || picture.index != index
            || picture.bits != output.len() * 8
            || picture.key != (index != 1)
        {
            return Err("retry changed the picture bytes, index or GOP".into());
        }
        if picture.key {
            sequence(cfg, &output)?;
        }
        row(work, &mut trace, 3, &[index, u64::from(picture.key)])?;
    }
    drop(encoder);
    if pool.used() != idle {
        return Err("encoder ownership leaked caller storage".into());
    }
    row(
        work,
        &mut trace,
        4,
        &[
            cfg.geometry.width() as u64,
            cfg.geometry.height() as u64,
            u64::from(cfg.max_rate.div_ceil(400)),
            u64::from(cfg.vbv_bits.div_ceil(16_384)),
        ],
    )
}

fn sequence(cfg: EncoderConfig, bytes: &[u8]) -> Result<(), String> {
    let header = bytes
        .strip_prefix(&[0, 0, 1, 0xb3])
        .and_then(<[u8]>::first_chunk::<8>)
        .ok_or("key picture has no sequence header")?;
    let word = u64::from_be_bytes(*header);
    if word >> 52 != cfg.geometry.width() as u64
        || (word >> 40) & 0xfff != cfg.geometry.height() as u64
        || (word >> 32) & 255 != 0x15
        || (word >> 14) & 0x3ffff != u64::from(cfg.max_rate.div_ceil(400))
        || (word >> 13) & 1 != 1
        || (word >> 3) & 0x3ff != u64::from(cfg.vbv_bits.div_ceil(16_384))
    {
        return Err("sequence header differs from the coded geometry, rate or VBV settings".into());
    }
    Ok(())
}

fn row(
    work: &mut Totals,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    words: &[u64],
) -> Result<(), String> {
    work.record(kind, words, &[]);
    if let Some(out) = trace {
        write!(out, "{kind}").map_err(explain)?;
        for word in words {
            write!(out, " {word}").map_err(explain)?;
        }
        writeln!(out).map_err(explain)?;
    }
    Ok(())
}

fn explain(error: impl std::fmt::Display) -> String {
    error.to_string()
}
