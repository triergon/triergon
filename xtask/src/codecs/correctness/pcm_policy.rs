//! Compare the PCM of a decoder's default and compatibility policies.

use std::{fs, path::Path};

use super::{Grade, RUN_TIMEOUT};
use crate::{
    json::Value,
    process::{Stop, run_text},
    python, sha256,
};

#[derive(Clone, Copy)]
pub(super) enum Policy {
    AacExtensions,
    DtsFilter,
}

impl Policy {
    pub(super) const fn for_lane(lane: &str) -> Option<Self> {
        match lane.as_bytes() {
            b"aac-extension-policy" => Some(Self::AacExtensions),
            b"dts-filter-policy" => Some(Self::DtsFilter),
            _ => None,
        }
    }

    pub(super) const fn manifest(self) -> &'static str {
        "harness/codecs/pcm-policies.json"
    }

    pub(super) fn compare(self, grade: &Grade, input: &Path, tmp: &Path) -> Result<Value, Stop> {
        let (decoder, options): (_, &[&str]) = match self {
            Self::AacExtensions => ("aac", &["--extensions", "reference"]),
            Self::DtsFilter => ("dts", &["--reference-prototype"]),
        };
        let executable = grade.example(decoder)?;
        let decode = |name: &str, options: &[&str]| {
            let pcm = tmp.join(name);
            let mut args = vec![executable.clone(), python::path(input), python::path(&pcm)];
            args.extend(options.iter().map(|arg| (*arg).to_owned()));
            let run = run_text(&args, RUN_TIMEOUT)?;
            if run.code != 0 {
                return Err(Stop::Fatal(format!("{decoder} {name}: {}", run.stderr)));
            }
            let bytes =
                fs::read(&pcm).map_err(|e| Stop::Fatal(format!("{}: {e}", pcm.display())))?;
            Ok((bytes, run))
        };
        let (candidate, candidate_run) = decode("default.f32", &[])?;
        let (reference, reference_run) = decode("compatibility.f32", options)?;
        if candidate.is_empty()
            || candidate.len() != reference.len()
            || candidate_run.stdout != reference_run.stdout
            || candidate_run.stderr != reference_run.stderr
        {
            return Err(Stop::Fatal(format!(
                "{decoder}: PCM extent or diagnostics differ between policies"
            )));
        }
        Ok(Value::object([
            ("ours_pcm_bytes", Value::int(candidate.len() as u64)),
            ("reference_pcm_bytes", Value::int(reference.len() as u64)),
            ("ours_pcm_sha256", Value::str(sha256::hex(&candidate))),
            ("reference_pcm_sha256", Value::str(sha256::hex(&reference))),
            (
                "status",
                Value::str(if candidate == reference {
                    "pass"
                } else {
                    "fail"
                }),
            ),
        ]))
    }
}
