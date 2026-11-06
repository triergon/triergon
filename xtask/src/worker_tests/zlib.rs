//! The candidate's inflate against streams and outputs of Python's zlib
//! (`python3 -m harness.oracles inflate`), and its malformed-stream and
//! resource-limit behaviour.

use std::time::Duration;

use super::{Context, Outcome, Scratch, finish, hex_bytes, items, unhex};
use crate::harness::oracle::{self, Kind};
use crate::harness::worker::Finished;
use crate::json::{self, Value};

/// The inflate cases of the zlib oracle.
fn cases(cx: &Context<'_>) -> Result<Value, String> {
    oracle::generate(cx.root, Kind::Inflate)?.map_err(|e| format!("inflate oracle: {e}"))
}

/// Inflate `data` with the given output, ratio and work limits; a run longer
/// than five seconds is an error.
fn invoke(cx: &Context<'_>, data: &[u8], limits: (u64, u64, u64)) -> Result<Finished, String> {
    let scratch = Scratch::new()?;
    let path = scratch.file("stream.zlib", data)?;
    let argv = super::argv(
        &cx.workers.candidate,
        "inflate",
        &path,
        &[&limits.0, &limits.1, &limits.2],
    );
    finish(&argv, Duration::from_secs(5))
}

const DEFAULT_LIMITS: (u64, u64, u64) = (1 << 20, 1024, 1 << 26);

/// The bytes of the hex JSON string a successful inflate prints.
fn inflated(result: &Finished) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(&result.stdout).map_err(|e| e.to_string())?;
    let value = json::parse(text).map_err(|e| e.to_string())?;
    unhex(value.as_str().ok_or("inflate output is not a string")?)
}

fn stderr(result: &Finished) -> String {
    String::from_utf8_lossy(&result.stderr).into_owned()
}

pub fn stored_fixed_dynamic_windows_blocks_and_overlap(cx: &mut Context<'_>) -> Outcome {
    let cases = cases(cx)?;
    let inputs = items(&cases, "inputs")?
        .iter()
        .map(|v| hex_bytes(Some(v)))
        .collect::<Result<Vec<_>, _>>()?;
    for stream in items(&cases, "streams")? {
        let field = |key: &str| stream.get(key).map_or_else(String::new, json::canonical);
        let label = format!(
            "window {} level {} {} input {}",
            field("window"),
            field("level"),
            field("strategy"),
            field("input")
        );
        let index = stream
            .get("input")
            .and_then(|v| match v {
                Value::Number(n) => n.as_i128(),
                _ => None,
            })
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| inputs.get(i))
            .ok_or_else(|| format!("{label}: unknown input"))?;
        let result = invoke(cx, &hex_bytes(stream.get("encoded"))?, DEFAULT_LIMITS)?;
        cx.same(
            result.code,
            0,
            format!("{label}: exit; {}", stderr(&result)),
        )?;
        cx.same(&inflated(&result)?, index, format!("{label}: output"))?;
    }
    Ok(())
}

pub fn bad_header_checksum_dictionary_truncation_and_limits(cx: &mut Context<'_>) -> Outcome {
    let data = hex_bytes(cases(cx)?.get("truncation"))?;
    for cut in 0..data.len() {
        let result = invoke(cx, &data[..cut], DEFAULT_LIMITS)?;
        cx.check(result.code != 0, || format!("a {cut}-byte prefix inflated"))?;
    }
    let last = data.len() - 1;
    let mut flipped = data.clone();
    flipped[last] ^= 1;
    let corrupt = [
        (
            "no compression method",
            [&[0x78, 0x00], &data[2..]].concat(),
        ),
        ("checksum", flipped),
        ("trailing bytes", [&data[..], b"extra"].concat()),
        ("preset dictionary", [&[0x78, 0x20], &data[2..]].concat()),
    ];
    for (what, encoded) in corrupt {
        let result = invoke(cx, &encoded, DEFAULT_LIMITS)?;
        cx.check(result.code != 0, || format!("{what}: inflated"))?;
    }
    for limits in [
        (100, 1024, 1 << 26),
        (1 << 20, 1, 1 << 26),
        (1 << 20, 1024, 1),
    ] {
        let result = invoke(cx, &data, limits)?;
        cx.check(result.code != 0, || format!("{limits:?}: inflated"))?;
        let message = stderr(&result);
        cx.check(message.contains("resource limit"), || {
            format!("{limits:?}: {message}")
        })?;
    }
    Ok(())
}

pub fn mutations_never_panic_or_hang(cx: &mut Context<'_>) -> Outcome {
    let cases = cases(cx)?;
    for (n, mutation) in items(&cases, "mutations")?.iter().enumerate() {
        let data = hex_bytes(mutation.get("data"))?;
        let result = invoke(cx, &data, (65536, 1024, 65536))?;
        let message = stderr(&result);
        cx.check(!message.contains("panicked"), || {
            format!("mutation {n}: {message}")
        })?;
        cx.check(!message.contains("signal"), || {
            format!("mutation {n}: {message}")
        })?;
        if result.code == 0 {
            let expected = match mutation.get("decoded") {
                Some(Value::Null) | None => {
                    return Err(format!(
                        "mutation {n}: inflated a stream Python's zlib rejects"
                    ));
                }
                decoded => hex_bytes(decoded)?,
            };
            cx.same(
                inflated(&result)?,
                expected,
                format!("mutation {n}: output"),
            )?;
        }
    }
    Ok(())
}
