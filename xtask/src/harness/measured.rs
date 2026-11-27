//! The measured record of a gate result: the bounds a status table shows, summarised
//! from the result's report when its evidence set is indexed, so the status page can be
//! generated from the evidence index alone.
//!
//! A record holds, for the cases the result timed, the largest runtime upper 95% bound,
//! the case it belongs to and how many cases hold under the policy, and for the cases
//! it measured the memory of, the largest heap and total RSS upper bounds and how many
//! cases hold both. Each part is absent when the result measured nothing of its kind.

use std::path::Path;

use super::bounds::{Bound, CaseBounds};
use super::policy::Policy;
use crate::json::{self, Value};

/// The runtime part of a measured record.
#[derive(Clone, Debug, PartialEq)]
pub struct Runtime {
    /// The largest valid upper bound and its case; `None` when no case has one.
    pub worst: Option<(f64, String)>,
    /// Cases whose upper bound holds under the policy.
    pub passing: u64,
    /// Cases timed.
    pub cases: u64,
}

/// The memory part of a measured record.
#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    /// The largest valid heap upper bound; `None` when no case has one.
    pub heap: Option<f64>,
    /// The largest valid total RSS upper bound; `None` when no case has one.
    pub total_rss: Option<f64>,
    /// Cases whose heap and total RSS bounds both hold under the policy.
    pub lower: u64,
    /// Cases measured.
    pub cases: u64,
}

/// What a gate result measured, as its index entry records it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measured {
    pub runtime: Option<Runtime>,
    pub memory: Option<Memory>,
}

impl Measured {
    /// The record of the graded `cases`; `None` when none measured anything.
    pub fn of_cases(cases: &[CaseBounds], policy: Policy) -> Option<Self> {
        let valid = |value: Option<f64>| value.filter(|v| v.is_finite());
        let largest = |bound: Bound| {
            cases
                .iter()
                .filter_map(|case| valid(case.value(bound)))
                .reduce(f64::max)
        };
        let holds = |case: &CaseBounds, bound: Bound| {
            case.value(bound)
                .is_some_and(|value| bound.holds(value, policy))
        };
        let timed: Vec<&CaseBounds> = cases
            .iter()
            .filter(|case| case.value(Bound::Runtime).is_some())
            .collect();
        let runtime = (!timed.is_empty()).then(|| Runtime {
            worst: timed.iter().fold(None, |worst, case| {
                match valid(case.value(Bound::Runtime)) {
                    Some(value) if worst.as_ref().is_none_or(|(most, _)| value > *most) => {
                        Some((value, case.id.clone()))
                    }
                    _ => worst,
                }
            }),
            passing: count(timed.iter().filter(|case| holds(case, Bound::Runtime))),
            cases: count(timed.iter()),
        });
        let measured: Vec<&CaseBounds> = cases
            .iter()
            .filter(|case| {
                case.value(Bound::Heap).is_some() || case.value(Bound::TotalRss).is_some()
            })
            .collect();
        let memory = (!measured.is_empty()).then(|| Memory {
            heap: largest(Bound::Heap),
            total_rss: largest(Bound::TotalRss),
            lower: count(
                measured
                    .iter()
                    .filter(|case| holds(case, Bound::Heap) && holds(case, Bound::TotalRss)),
            ),
            cases: count(measured.iter()),
        });
        (runtime.is_some() || memory.is_some()).then_some(Self { runtime, memory })
    }

    /// The record as an index entry holds it.
    pub fn to_value(&self) -> Value {
        let number = |value: Option<f64>| value.map_or(Value::Null, Value::float);
        let mut fields = Vec::new();
        if let Some(runtime) = &self.runtime {
            let (upper, worst) = match &runtime.worst {
                Some((upper, id)) => (Value::float(*upper), Value::str(id)),
                None => (Value::Null, Value::Null),
            };
            fields.push((
                "runtime",
                Value::object([
                    ("upper", upper),
                    ("worst", worst),
                    ("passing", Value::int(runtime.passing)),
                    ("cases", Value::int(runtime.cases)),
                ]),
            ));
        }
        if let Some(memory) = &self.memory {
            fields.push((
                "memory",
                Value::object([
                    ("heap", number(memory.heap)),
                    ("total_rss", number(memory.total_rss)),
                    ("lower", Value::int(memory.lower)),
                    ("cases", Value::int(memory.cases)),
                ]),
            ));
        }
        Value::object(fields)
    }

    /// The record an index entry holds.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let Value::Object(members) = value else {
            return Err("a measured record is an object".into());
        };
        if let Some((key, _)) = members
            .iter()
            .find(|(key, _)| key != "runtime" && key != "memory")
        {
            return Err(format!("a measured record has no field {key}"));
        }
        let count = |part: &Value, key: &str| {
            part.get(key)
                .and_then(|n| match n {
                    Value::Number(number) => number.as_i128(),
                    _ => None,
                })
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| format!("a measured record's {key} is a count"))
        };
        let bound = |part: &Value, key: &str| match part.get(key) {
            Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) if number.as_f64().is_finite() => Ok(Some(number.as_f64())),
            _ => Err(format!(
                "a measured record's {key} is a finite bound or null"
            )),
        };
        let runtime = value
            .get("runtime")
            .map(|part| {
                let worst = match (bound(part, "upper")?, part.get("worst")) {
                    (Some(upper), Some(Value::String(id))) => Some((upper, id.clone())),
                    (None, Some(Value::Null)) => None,
                    _ => {
                        return Err("a measured runtime names its worst case with its bound".into());
                    }
                };
                let (passing, cases) = (count(part, "passing")?, count(part, "cases")?);
                if passing > cases {
                    return Err("a measured runtime passes more cases than it timed".into());
                }
                Ok::<_, String>(Runtime {
                    worst,
                    passing,
                    cases,
                })
            })
            .transpose()?;
        let memory = value
            .get("memory")
            .map(|part| {
                let (lower, cases) = (count(part, "lower")?, count(part, "cases")?);
                if lower > cases {
                    return Err("a measured memory holds more cases than it measured".into());
                }
                Ok::<_, String>(Memory {
                    heap: bound(part, "heap")?,
                    total_rss: bound(part, "total_rss")?,
                    lower,
                    cases,
                })
            })
            .transpose()?;
        Ok(Self { runtime, memory })
    }

    /// The record of several results together: the largest bounds, and the counts summed.
    pub fn join<'a>(records: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut joined = Self::default();
        for record in records {
            if let Some(runtime) = &record.runtime {
                let into = joined.runtime.get_or_insert(Runtime {
                    worst: None,
                    passing: 0,
                    cases: 0,
                });
                if let Some((upper, id)) = &runtime.worst
                    && into.worst.as_ref().is_none_or(|(most, _)| upper > most)
                {
                    into.worst = Some((*upper, id.clone()));
                }
                into.passing += runtime.passing;
                into.cases += runtime.cases;
            }
            if let Some(memory) = &record.memory {
                let into = joined.memory.get_or_insert(Memory {
                    heap: None,
                    total_rss: None,
                    lower: 0,
                    cases: 0,
                });
                let larger = |a: Option<f64>, b: Option<f64>| match (a, b) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                into.heap = larger(into.heap, memory.heap);
                into.total_rss = larger(into.total_rss, memory.total_rss);
                into.lower += memory.lower;
                into.cases += memory.cases;
            }
        }
        joined
    }
}

fn count<T>(items: impl Iterator<Item = T>) -> u64 {
    items.count() as u64
}

/// Where a gate command's measurements lie in its output directory, by the command its
/// gate result names; `None` for a command that measures nothing (`gate test`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `report.json`, whose `cases` (or one `case`) are graded rows: the report
    /// runner's collections and grades, the ISO BMFF suites and the external vector.
    Report,
    /// One record per workload beside the gate result (`codecs time`).
    CodecTimes,
    /// The workload lines of `summary.json` (`codecs memory`).
    CodecMemory,
    /// One `CASE/result.json` per case (`capture qualify`); the timing stage writes none.
    Capture,
}

impl Source {
    pub fn of(command: &str) -> Option<Self> {
        match command {
            "gate test" => None,
            "codecs time" => Some(Self::CodecTimes),
            "codecs memory" => Some(Self::CodecMemory),
            "capture qualify" => Some(Self::Capture),
            _ => Some(Self::Report),
        }
    }

    /// Whether the file at `relative`, below the result's directory, is one the
    /// record is read from.
    pub fn reads(self, relative: &str) -> bool {
        let parts: Vec<&str> = relative.split('/').collect();
        match (self, parts.as_slice()) {
            (Self::Report, ["report.json"]) | (Self::CodecMemory, ["summary.json"]) => true,
            (Self::CodecTimes, [file]) => {
                Path::new(file).extension().is_some_and(|e| e == "json")
                    && *file != super::reuse::RESULT
                    && *file != "identity.json"
            }
            (Self::Capture, [_, "result.json"]) => true,
            _ => false,
        }
    }

    /// The graded rows of a file the source reads, at `path` below the result's
    /// directory.
    pub fn rows(self, path: &str, data: &[u8]) -> Result<Vec<Value>, String> {
        let text = std::str::from_utf8(data).map_err(|_| "not UTF-8".to_owned())?;
        let document = json::parse(text).map_err(|e| e.to_string())?;
        let mut rows = Vec::new();
        {
            match self {
                Self::Report => match (document.get("cases"), document.get("case")) {
                    (Some(Value::Array(cases)), _) => rows.extend(cases.iter().cloned()),
                    (_, Some(case @ Value::Object(_))) => rows.push(case.clone()),
                    _ => {}
                },
                Self::CodecTimes => {
                    if let (Some(id), Some(status)) = (document.get("id"), document.get("status"))
                        && document.get("operation").is_some()
                    {
                        rows.push(Value::object([
                            ("id", id.clone()),
                            (
                                "timing",
                                Value::object([
                                    ("status", status.clone()),
                                    ("ratio", field(&document, "ratio")),
                                ]),
                            ),
                        ]));
                    }
                }
                Self::CodecMemory => {
                    for line in document.as_array().unwrap_or_default() {
                        rows.push(Value::object([
                            ("id", field(line, "id")),
                            (
                                "memory",
                                Value::object([
                                    ("status", field(line, "memory_status")),
                                    ("heap_ratio_upper_95", field(line, "heap_ratio_upper_95")),
                                    (
                                        "total_rss_ratio_upper_95",
                                        field(line, "total_rss_ratio_upper_95"),
                                    ),
                                ]),
                            ),
                        ]));
                    }
                }
                Self::Capture => {
                    let case = Path::new(path)
                        .parent()
                        .and_then(Path::file_name)
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let ratio = document
                        .get("runtime")
                        .map_or(Value::Null, |runtime| field(runtime, "ratio"));
                    let timed = ratio
                        .get("upper_95")
                        .and_then(Value::as_f64)
                        .is_some_and(f64::is_finite);
                    rows.push(Value::object([
                        ("id", Value::str(case)),
                        (
                            "timing",
                            Value::object([
                                (
                                    "status",
                                    Value::str(if timed { "measured" } else { "invalid-timing" }),
                                ),
                                ("ratio", ratio),
                            ]),
                        ),
                        ("memory", field(&document, "memory")),
                    ]));
                }
            }
        }
        Ok(rows)
    }
}

fn field(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests;
