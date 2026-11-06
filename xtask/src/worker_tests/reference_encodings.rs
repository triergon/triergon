//! Content encodings the Matroska reference does not cover alone, compared
//! through the combined reference against the exact source oracle, and the
//! explicit capability bound of that reference.

use super::content::{replace_all, zlib_fixture};
use super::{Context, Outcome, Scratch, TIMEOUT, argv, expected, finish, output};
use crate::corpus::{Kind as Deflate, ZlibCase};
use crate::ebml::{children, element};
use crate::harness::oracle::Kind;
use crate::harness::parity::projected_content;
use crate::json::Value;

/// The payload range of the first child `id` of `data[begin..end]`.
fn child(data: &[u8], begin: usize, end: usize, id: u32) -> Result<(usize, usize), String> {
    children(data, begin, end)?
        .into_iter()
        .find(|c| c.id == id)
        .map(|c| (c.start, c.end))
        .ok_or_else(|| format!("no element {id:#x}"))
}

/// Deflate kinds, lacing, empty frames and both storage orders of the encoding
/// stages, each compared with the source oracle through both workers.
pub fn deflate_lacing_empty_and_encoding_storage_order_against_source_oracle(
    cx: &mut Context<'_>,
) -> Outcome {
    let reference = cx.workers.combined.clone();
    let candidate = cx.workers.candidate.clone();
    let scratch = Scratch::new()?;
    for kind in [Deflate::Stored, Deflate::Fixed, Deflate::Dynamic] {
        for lace in 0..4 {
            for empty in [false, true] {
                let data = zlib_fixture(
                    cx.root,
                    ZlibCase {
                        kind,
                        lace,
                        empty,
                        mixed: true,
                        ..ZlibCase::default()
                    },
                )?;
                let (a, b) = child(&data, 0, data.len(), 0x18538067)?;
                let (a, b) = child(&data, a, b, 0x1654ae6b)?;
                let (a, b) = child(&data, a, b, 0xae)?;
                let (a, b) = child(&data, a, b, 0x6d80)?;
                let stages: Vec<Vec<u8>> = children(&data, a, b)?
                    .iter()
                    .map(|c| element(c.id, &data[c.start..c.end]))
                    .collect();
                let stored = element(0x6d80, &stages.concat());
                for reverse in [false, true] {
                    let label = format!("{kind:?} lace {lace} empty {empty} reverse {reverse}");
                    let mut order = stages.clone();
                    if reverse {
                        order.reverse();
                    }
                    let alternate = element(0x6d80, &order.concat());
                    let source = replace_all(&data, &stored, &alternate);
                    cx.same(source.len(), data.len(), format!("{label}: length"))?;
                    let path = scratch.file("mixed.mkv", &source)?;
                    let oracle = expected(cx.root, Kind::Content, &path)?;
                    let native = output(&argv(&reference, "trace", &path, &[]))?;
                    let rust = output(&argv(&candidate, "trace", &path, &[]))?;
                    cx.equal(
                        &projected_content(&native).unwrap_or(Value::Null),
                        &oracle,
                        format!("{label}: reference against the oracle"),
                    )?;
                    cx.equal(
                        &projected_content(&rust).unwrap_or(Value::Null),
                        &oracle,
                        format!("{label}: candidate against the oracle"),
                    )?;
                    cx.equal(
                        &native,
                        &rust,
                        format!("{label}: reference against candidate"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

pub fn combined_private_scope_is_an_explicit_unsupported_reference_case(
    cx: &mut Context<'_>,
) -> Outcome {
    let scratch = Scratch::new()?;
    let data = zlib_fixture(
        cx.root,
        ZlibCase {
            mixed: true,
            scope: 3,
            ..ZlibCase::default()
        },
    )?;
    let path = scratch.file("private.mkv", &data)?;
    let result = finish(&argv(&cx.workers.combined, "trace", &path, &[]), TIMEOUT)?;
    cx.check(result.code != 0, || {
        "the combined reference accepted private-data compression".into()
    })
}
