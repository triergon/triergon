//! Content encoding semantics against the independent Python and zlib oracles.

use super::{
    Context, Outcome, Scratch, argv, case_file, corpus_cases, expected, hex_bytes, items, output,
};
use crate::corpus::{self, ZlibCase};
use crate::harness::oracle::Kind;
use crate::harness::parity::projected_content;
use crate::json::Value;

/// One authored zlib corpus variant (`cargo xtask fixture zlib`).
pub fn zlib_fixture(root: &std::path::Path, case: ZlibCase) -> Result<Vec<u8>, String> {
    corpus::zlib_case(&corpus::Source::load(root)?, case)
}

pub fn ordered_chain_empty_and_private_bytes_against_independent_oracle(
    cx: &mut Context<'_>,
) -> Outcome {
    let cases = [
        ZlibCase {
            empty: true,
            ..ZlibCase::default()
        },
        ZlibCase {
            mixed: true,
            ..ZlibCase::default()
        },
        ZlibCase {
            scope: 2,
            aac: true,
            ..ZlibCase::default()
        },
        ZlibCase {
            scope: 3,
            ..ZlibCase::default()
        },
    ];
    let scratch = Scratch::new()?;
    for case in cases {
        let path = scratch.file("encoded.mkv", &zlib_fixture(cx.root, case)?)?;
        let chain = expected(cx.root, Kind::Chain, &path)?;
        let private = hex_bytes(chain.get("private"))?;
        let frames = items(&chain, "frames")?
            .iter()
            .map(|f| hex_bytes(Some(f)))
            .collect::<Result<Vec<_>, _>>()?;
        let trace = output(&argv(&cx.workers.candidate, "trace", &path, &[]))?;
        let tracks = items(&trace, "tracks")?;
        let first = tracks.first().ok_or("trace without tracks")?;
        cx.same(
            hex_bytes(first.get("private"))?,
            private,
            format!("{case:?}: private"),
        )?;
        let payloads = items(&trace, "packets")?
            .iter()
            .map(|p| hex_bytes(p.get("payload")))
            .collect::<Result<Vec<_>, _>>()?;
        cx.same(payloads, frames, format!("{case:?}: frames"))?;
    }
    Ok(())
}

pub fn optional_pcm_decoder_view_preserves_the_full_source_event(cx: &mut Context<'_>) -> Outcome {
    let scratch = Scratch::new()?;
    let empty = zlib_fixture(
        cx.root,
        ZlibCase {
            empty: true,
            ..ZlibCase::default()
        },
    )?;
    let path = scratch.file("empty.mkv", &empty)?;
    let worker = cx.workers.candidate.clone();
    let source = output(&argv(&worker, "trace", &path, &[]))?;
    let timing = output(&argv(&worker, "trace-source", &path, &[]))?;
    let projected = output(&argv(&worker, "codec-trace", &path, &[]))?;
    let native = output(&argv(&cx.workers.reference, "codec-trace", &path, &[]))?;
    let source_packets = items(&source, "packets")?;
    cx.same(source_packets.len(), 1, "source packets")?;
    cx.equal(
        source_packets[0].get("payload").unwrap_or(&Value::Null),
        &Value::str(""),
        "source payload",
    )?;
    cx.same(items(&timing, "packets")?.len(), 1, "timing packets")?;
    cx.equal(
        &projected,
        &native,
        "candidate codec view against reference",
    )?;
    cx.equal(
        projected.get("packets").unwrap_or(&Value::Null),
        &Value::Array(Vec::new()),
        "codec view packets",
    )?;
    // This is a narrow PCM view, never a universal empty-frame filter.
    let big = replace_all(&empty, b"A_PCM/INT/LIT", b"A_PCM/INT/BIG");
    let path = scratch.file("empty.mkv", &big)?;
    let opaque = output(&argv(&worker, "codec-trace", &path, &[]))?;
    cx.same(
        items(&opaque, "packets")?.len(),
        1,
        "big-endian PCM packets",
    )
}

pub fn all_selected_compression_bytes_match_an_independent_source_oracle(
    cx: &mut Context<'_>,
) -> Outcome {
    for case in corpus_cases(cx.root, "matroska-zlib")? {
        let path = case_file(cx.root, "zlib-fixtures", &case)?;
        let id = case.get("id").and_then(Value::as_str).unwrap_or("");
        let trace = output(&argv(&cx.workers.candidate, "trace", &path, &[]))?;
        let projected = projected_content(&trace).unwrap_or(Value::Null);
        let oracle = expected(cx.root, Kind::Content, &path)?;
        cx.equal(&projected, &oracle, id)?;
    }
    Ok(())
}

/// `data` with every occurrence of `from` replaced by `to` (Python `bytes.replace`).
pub fn replace_all(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0;
    while at < data.len() {
        if !from.is_empty() && data[at..].starts_with(from) {
            out.extend_from_slice(to);
            at += from.len();
        } else {
            out.push(data[at]);
            at += 1;
        }
    }
    out
}
