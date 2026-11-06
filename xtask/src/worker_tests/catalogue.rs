//! Whole-input catalogue and source parity, and complete measured catalogue work.

use super::checksum::check_work;
use super::{Context, Outcome, Scratch, argv, case_file, corpus_cases, execute, expected};
use crate::harness::oracle::Kind;
use crate::harness::parity::{Run, catalogue_parity};
use crate::harness::worker::data;
use crate::json::Value;

pub fn native_catalogue_exact_source_and_all_checksum_work(cx: &mut Context<'_>) -> Outcome {
    for case in corpus_cases(cx.root, "matroska-segment-metadata")? {
        let path = case_file(cx.root, "catalogue-fixtures", &case)?;
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        cx.subtest(format!("case={stem}"), |cx| {
            let workers = cx.workers;
            let (candidate, reference) = (&workers.candidate, &workers.reference);
            let left = execute(&argv(candidate, "catalogue-trace", &path, &[]));
            let right = execute(&argv(reference, "catalogue-trace", &path, &[]));
            cx.ok(&left, "candidate catalogue-trace")?;
            cx.ok(&right, "reference catalogue-trace")?;
            cx.no_differences(data(&left), data(&right), "candidate against reference")?;
            let source = execute(&argv(candidate, "catalogue-source", &path, &[]));
            cx.ok(&source, "catalogue-source")?;
            let oracle = expected(cx.root, Kind::Catalogue, &path)?;
            cx.no_differences(
                data(&source),
                &oracle,
                "source against the catalogue oracle",
            )?;
            for worker in [candidate, reference] {
                let work = execute(&argv(worker, "catalogue-bench", &path, &[&1]));
                let name = worker.file_name().unwrap_or_default().to_string_lossy();
                cx.ok(&work, format!("{name} catalogue-bench"))?;
                check_work(cx, data(&work), data(&left), &name)?;
            }
            Ok(())
        });
    }
    Ok(())
}

pub fn original_hdr_vector_is_not_stripped_and_matches_complete_protocol(
    cx: &mut Context<'_>,
) -> Outcome {
    let path = cx
        .root
        .join(".research/fate-samples/mkv/hdr10tags-both.mkv");
    cx.check(path.is_file(), || {
        "fetch the original required HDR FATE vector before this gate".into()
    })?;
    let scratch = Scratch::new()?;
    let workers = cx.workers;
    let run = Run {
        root: cx.root,
        candidate: &workers.candidate,
        reference: &workers.reference,
        input: &path,
        raw: scratch.path().to_path_buf(),
    };
    let result = catalogue_parity(&run, true, None, false)?;
    let status = |key: &str| result.get(key).cloned().unwrap_or(Value::Null);
    cx.equal(&status("status"), &Value::str("pass"), "parity status")?;
    cx.equal(
        &status("source_catalogue_status"),
        &Value::str("pass"),
        "source catalogue status",
    )?;
    cx.equal(
        &status("source_colour_status"),
        &Value::str("pass"),
        "source colour status",
    )
}
