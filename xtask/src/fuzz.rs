//! Deterministic, replayable byte mutations run through JSON workers, with
//! bounded, ungraded observations. A campaign never grants feature, speed,
//! memory or module acceptance: it records outcomes and disagreements.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::harness::diff::{equal, python_str_len};
use crate::json::{self, Value};
use crate::pyrandom::PyRandom;
use crate::sha256::hex as digest_bytes;

type R<T> = Result<T, String>;

const MAX_JSON_DEPTH: usize = 256;
const MAX_JSON_NODES: usize = 1_000_000;

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn py_int(value: Option<&Value>) -> Option<i128> {
    crate::harness::linux::py_int(value)
}

/// Bound depth and node count of a parsed document.
pub fn validate_shape(value: &Value) -> R<()> {
    let mut stack: Vec<(&Value, usize)> = vec![(value, 0)];
    let mut nodes = 0;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_JSON_NODES {
            return Err("JSON node limit exceeded".into());
        }
        let children: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            Value::Object(members) => members.iter().map(|(_, v)| v).collect(),
            _ => continue,
        };
        if depth >= MAX_JSON_DEPTH {
            return Err("JSON nesting limit exceeded".into());
        }
        stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
    }
    Ok(())
}

/// Strict worker-protocol JSON: exact decimals, no duplicate keys, no
/// non-finite numbers, bounded depth and size.
pub fn strict_json(bytes: &[u8]) -> R<Value> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let value = json::parse_strict(text).map_err(|e| e.to_string())?;
    validate_shape(&value)?;
    Ok(value)
}

/// Tag every non-integer number (an exact decimal) as `{"json_decimal": text}`.
pub fn tag_decimals(value: Value) -> Value {
    match value {
        Value::Number(n) if !n.is_integer() => {
            Value::object([("json_decimal", Value::str(json::decimal_text(n.text())))])
        }
        Value::Array(items) => Value::Array(items.into_iter().map(tag_decimals).collect()),
        Value::Object(members) => Value::Object(
            members
                .into_iter()
                .map(|(k, v)| (k, tag_decimals(v)))
                .collect(),
        ),
        other => other,
    }
}

fn shown(value: &Value) -> Value {
    match value {
        Value::Number(n) if !n.is_integer() => value.clone(),
        Value::Array(_) | Value::Object(_) => value.clone(),
        _ if python_str_len(value, true) < 256 => value.clone(),
        _ => Value::str("<large value>"),
    }
}

fn sorted_keys(members: &[(String, Value)]) -> Vec<&str> {
    let mut keys: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Typed differences of exact-decimal documents, decimals kept exact.
fn differences(actual: &Value, expected: &Value) -> Vec<Value> {
    fn walk(a: &Value, b: &Value, at: &str, found: &mut Vec<Value>) {
        if found.len() >= 40 {
            return;
        }
        let is_bool = |v: &Value| matches!(v, Value::Bool(_));
        match (a, b) {
            _ if is_bool(a) != is_bool(b) => found.push(Value::object([
                ("path", Value::str(at)),
                ("actual", shown(a)),
                ("expected", shown(b)),
            ])),
            (Value::Object(x), Value::Object(y)) => {
                let (kx, ky) = (sorted_keys(x), sorted_keys(y));
                if kx != ky {
                    found.push(Value::object([
                        ("path", Value::str(at)),
                        ("actual_keys", Value::strings(kx.iter().copied())),
                        ("expected_keys", Value::strings(ky.iter().copied())),
                    ]));
                }
                for key in kx.iter().filter(|k| ky.contains(k)) {
                    walk(
                        a.get(key).unwrap(),
                        b.get(key).unwrap(),
                        &format!("{at}/{key}"),
                        found,
                    );
                }
            }
            (Value::Array(x), Value::Array(y)) => {
                if x.len() != y.len() {
                    found.push(Value::object([
                        ("path", Value::str(at)),
                        ("actual_length", Value::int(x.len() as i64)),
                        ("expected_length", Value::int(y.len() as i64)),
                    ]));
                }
                for (i, (l, r)) in x.iter().zip(y).enumerate() {
                    walk(l, r, &format!("{at}/{i}"), found);
                }
            }
            _ if !equal(a, b) => found.push(Value::object([
                ("path", Value::str(at)),
                ("actual", shown(a)),
                ("expected", shown(b)),
            ])),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(actual, expected, "", &mut found);
    found
}

/// Apply a recorded edit list to `seed`, checking every bound and both hashes.
pub fn replay(seed: &[u8], recipe: &Value, maximum_bytes: usize) -> R<Vec<u8>> {
    if py_int(recipe.get("schema")) != Some(1)
        || s(recipe, "seed_sha256") != Some(digest_bytes(seed).as_str())
    {
        return Err("mutation recipe does not bind this seed".into());
    }
    if maximum_bytes < seed.len() || maximum_bytes < 1 {
        return Err("invalid mutation input bound".into());
    }
    let edits = recipe
        .get("edits")
        .and_then(Value::as_array)
        .filter(|e| (1..=8).contains(&e.len()))
        .ok_or("need one to eight bounded edits")?;
    let mut result = seed.to_vec();
    for edit in edits {
        let Value::Object(fields) = edit else {
            return Err("unknown mutation edit fields".into());
        };
        let mut keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        if keys != ["insert_hex", "offset", "remove"] {
            return Err("unknown mutation edit fields".into());
        }
        let (offset, remove) = (py_int(edit.get("offset")), py_int(edit.get("remove")));
        let len = result.len() as i128;
        let (Some(offset), Some(remove)) = (offset, remove) else {
            return Err("mutation edit is outside its current input".into());
        };
        if !(0..=len).contains(&offset) || !(0..=len - offset).contains(&remove) {
            return Err("mutation edit is outside its current input".into());
        }
        let encoded = s(edit, "insert_hex").ok_or("invalid mutation bytes")?;
        if encoded.len() % 2 != 0 || encoded.len() > 2 * maximum_bytes {
            return Err("invalid mutation bytes".into());
        }
        if !encoded.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid mutation hexadecimal data".into());
        }
        if result.len() - remove as usize + encoded.len() / 2 > maximum_bytes {
            return Err("mutation exceeds its declared input bound".into());
        }
        let inserted: Vec<u8> = (0..encoded.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).unwrap())
            .collect();
        let (offset, remove) = (offset as usize, remove as usize);
        result.splice(offset..offset + remove, inserted);
    }
    if s(recipe, "result_sha256") != Some(digest_bytes(&result).as_str()) {
        return Err("mutation result hash disagrees with its recipe".into());
    }
    Ok(result)
}

impl PyRandom {
    /// `randint(a, b)`.
    fn randint(&mut self, a: usize, b: usize) -> usize {
        a + self.below((b - a + 1) as u32) as usize
    }

    /// `randrange(n)`.
    fn randrange(&mut self, n: usize) -> usize {
        self.below(n as u32) as usize
    }
}

/// One to `maximum_edits` random edits (flip, delete, truncate, overwrite with
/// marker bytes, duplicate, insert), returned with their replayable recipe.
pub fn mutation(
    seed: &[u8],
    rng: &mut PyRandom,
    maximum_bytes: usize,
    maximum_edits: usize,
) -> R<(Vec<u8>, Value)> {
    if !(1..=7).contains(&maximum_edits) || seed.len() > maximum_bytes || maximum_bytes < 1 {
        return Err("invalid mutation generation bounds".into());
    }
    let mut data = seed.to_vec();
    let mut edits = Vec::new();
    let mut apply = |data: &mut Vec<u8>, offset: usize, remove: usize, inserted: Vec<u8>| {
        edits.push(Value::object([
            ("offset", Value::int(offset as i64)),
            ("remove", Value::int(remove as i64)),
            (
                "insert_hex",
                Value::str(
                    inserted
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                ),
            ),
        ]));
        data.splice(offset..offset + remove, inserted);
    };
    for _ in 0..rng.randint(1, maximum_edits) {
        let mut choices: Vec<&str> = if data.is_empty() {
            vec!["insert"]
        } else {
            vec!["flip", "delete", "truncate", "overwrite", "duplicate"]
        };
        if data.len() < maximum_bytes && !data.is_empty() {
            choices.push("insert");
        }
        let kind = choices[rng.randrange(choices.len())];
        match kind {
            "flip" => {
                let offset = rng.randrange(data.len());
                let flipped = data[offset] ^ (1 << rng.randrange(8));
                apply(&mut data, offset, 1, vec![flipped]);
            }
            "delete" => {
                let offset = rng.randrange(data.len());
                let count = rng.randint(1, 4096.min(data.len() - offset));
                apply(&mut data, offset, count, vec![]);
            }
            "truncate" => {
                let offset = rng.randrange(data.len());
                let count = data.len() - offset;
                apply(&mut data, offset, count, vec![]);
            }
            "overwrite" => {
                let offset = rng.randrange(data.len());
                let count = rng.randint(1, 8.min(data.len() - offset));
                // VINT markers, unknown lengths, signed bounds and zeros.
                let value = [0u8, 0x7f, 0x80, 0xff][rng.randrange(4)];
                apply(&mut data, offset, count, vec![value; count]);
            }
            "duplicate" if data.len() < maximum_bytes => {
                let start = rng.randrange(data.len());
                let count = rng.randint(
                    1,
                    4096.min(data.len() - start).min(maximum_bytes - data.len()),
                );
                let copy = data[start..start + count].to_vec();
                let at = rng.randrange(data.len() + 1);
                apply(&mut data, at, 0, copy);
            }
            _ => {
                let capacity = 16.min(maximum_bytes - data.len());
                if capacity == 0 {
                    let offset = rng.randrange(data.len());
                    let flipped = data[offset] ^ 0x80;
                    apply(&mut data, offset, 1, vec![flipped]);
                } else {
                    let count = rng.randint(1, capacity);
                    let at = rng.randrange(data.len() + 1);
                    let inserted: Vec<u8> = (0..count).map(|_| rng.randrange(256) as u8).collect();
                    apply(&mut data, at, 0, inserted);
                }
            }
        }
    }
    if data == seed {
        if data.is_empty() {
            apply(&mut data, 0, 0, vec![0x80]);
        } else {
            let flipped = data[0] ^ 1;
            apply(&mut data, 0, 1, vec![flipped]);
        }
    }
    let recipe = Value::object([
        ("schema", Value::int(1)),
        ("seed_sha256", Value::str(digest_bytes(seed))),
        ("edits", Value::Array(edits)),
        ("result_sha256", Value::str(digest_bytes(&data))),
    ]);
    if replay(seed, &recipe, maximum_bytes)? != data {
        return Err("generated mutation is not reproducible".into());
    }
    Ok((data, recipe))
}

fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

struct Streams {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    seen: usize,
    exceeded: bool,
}

/// Run one worker in its own process group with an elapsed-time and combined
/// output limit; the group is killed on a limit and always at the end. This is
/// neither a memory sandbox nor a timing observation.
pub fn observe(
    command: &[OsString],
    timeout: Duration,
    maximum_output: usize,
) -> R<(Value, Vec<u8>, Vec<u8>)> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    if timeout.is_zero() || maximum_output < 1 {
        return Err("invalid worker observation bounds".into());
    }
    let started = Instant::now();
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("{}: {e}", command[0].to_string_lossy()))?;
    let pid = child.id();
    let streams = Arc::new(Mutex::new(Streams {
        stdout: Vec::new(),
        stderr: Vec::new(),
        seen: 0,
        exceeded: false,
    }));
    let reader = |mut pipe: Box<dyn Read + Send>, stdout: bool| {
        let streams = Arc::clone(&streams);
        std::thread::spawn(move || {
            let mut block = vec![0u8; 65536];
            loop {
                let n = match pipe.read(&mut block) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut st = streams.lock().expect("stream state");
                if st.exceeded {
                    break;
                }
                let available = maximum_output.saturating_sub(st.seen).min(n);
                if stdout {
                    st.stdout.extend_from_slice(&block[..available]);
                } else {
                    st.stderr.extend_from_slice(&block[..available]);
                }
                st.seen += n;
                if st.seen > maximum_output {
                    st.exceeded = true;
                    break;
                }
            }
        })
    };
    let out = reader(Box::new(child.stdout.take().expect("stdout")), true);
    let err = reader(Box::new(child.stderr.take().expect("stderr")), false);
    let mut state = "completed";
    let mut code = None;
    loop {
        if streams.lock().expect("stream state").exceeded {
            state = "output-limit";
            break;
        }
        if out.is_finished() && err.is_finished() {
            break;
        }
        if started.elapsed() >= timeout {
            state = "timeout";
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if state != "completed" {
        kill_group(pid);
    }
    let deadline = Instant::now()
        + if state == "completed" {
            timeout
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(1))
        } else {
            Duration::from_secs(5)
        };
    while code.is_none() {
        match child.try_wait() {
            Ok(Some(status)) => {
                code = Some(
                    status
                        .code()
                        .map_or_else(|| -i64::from(status.signal().unwrap_or(0)), i64::from),
                );
            }
            Ok(None) if Instant::now() >= deadline => {
                state = "timeout";
                kill_group(pid);
                let status = child.wait().map_err(|e| e.to_string())?;
                code = Some(
                    status
                        .code()
                        .map_or_else(|| -i64::from(status.signal().unwrap_or(0)), i64::from),
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(e.to_string()),
        }
    }
    // The leader may be gone while descendants in its group still hold pipes.
    kill_group(pid);
    let _ = (out.join(), err.join());
    let st = streams.lock().expect("stream state");
    let (stdout, stderr) = (st.stdout.clone(), st.stderr.clone());
    let code = code.unwrap_or(0);
    let mut result = vec![
        ("status".to_owned(), Value::str(state)),
        ("returncode".to_owned(), Value::int(code)),
        (
            "command".to_owned(),
            Value::strings(command.iter().map(|c| c.to_string_lossy().into_owned())),
        ),
        (
            "stdout_sha256".to_owned(),
            Value::str(digest_bytes(&stdout)),
        ),
        (
            "stderr_sha256".to_owned(),
            Value::str(digest_bytes(&stderr)),
        ),
        (
            "output_truncated".to_owned(),
            Value::Bool(state == "output-limit"),
        ),
        ("output_bytes_seen".to_owned(), Value::int(st.seen as i64)),
        (
            "elapsed_seconds".to_owned(),
            Value::float(started.elapsed().as_secs_f64()),
        ),
    ];
    if state == "completed" {
        let status = if code < 0 || code == 101 {
            "crash"
        } else if code != 0 {
            "rejected"
        } else {
            match strict_json(&stdout) {
                Ok(data) => {
                    result.push(("data".into(), data));
                    "ok"
                }
                Err(error) => {
                    result.push(("protocol_error".into(), Value::str(error)));
                    "invalid-json"
                }
            }
        };
        result[0].1 = Value::str(status);
    }
    Ok((Value::Object(result), stdout, stderr))
}

/// Differential outcome of one operation; rejections and incomplete runs stay
/// ungraded, never accepted.
pub fn compare(outcomes: &[(String, Value)]) -> R<Value> {
    let candidate = outcomes
        .iter()
        .find(|(k, _)| k == "candidate")
        .map(|(_, v)| v)
        .ok_or("a differential observation needs candidate and another role")?;
    if outcomes.len() < 2 {
        return Err("a differential observation needs candidate and another role".into());
    }
    let (mut findings, mut comparisons) = (Vec::new(), Vec::new());
    for (role, result) in outcomes {
        let status = s(result, "status").unwrap_or_default();
        if !matches!(status, "ok" | "rejected") {
            findings.push(Value::object([
                ("kind", Value::str(format!("worker-{status}"))),
                ("role", Value::str(role)),
            ]));
        }
    }
    let mut others: Vec<&(String, Value)> =
        outcomes.iter().filter(|(k, _)| k != "candidate").collect();
    others.sort_by(|a, b| a.0.cmp(&b.0));
    for (role, other) in others {
        let (left, right) = (
            s(candidate, "status").unwrap_or_default(),
            s(other, "status").unwrap_or_default(),
        );
        if left == "ok" && right == "ok" {
            let (a, b) = (
                candidate.get("data").unwrap_or(&Value::Null),
                other.get("data").unwrap_or(&Value::Null),
            );
            if let Err(error) = validate_shape(a).and_then(|()| validate_shape(b)) {
                comparisons.push(Value::object([
                    ("role", Value::str(role)),
                    ("status", Value::str("comparison-limit")),
                    ("reason", Value::str(&error)),
                ]));
                findings.push(Value::object([
                    ("kind", Value::str("comparison-limit")),
                    ("role", Value::str(role)),
                    ("reason", Value::str(error)),
                ]));
                continue;
            }
            let delta = tag_decimals(Value::Array(differences(a, b)));
            let different = delta.truthy();
            comparisons.push(Value::object([
                ("role", Value::str(role)),
                (
                    "status",
                    Value::str(if different {
                        "different"
                    } else {
                        "observed-agreement"
                    }),
                ),
                ("differences", delta.clone()),
            ]));
            if different {
                findings.push(Value::object([
                    ("kind", Value::str("output-disagreement")),
                    ("role", Value::str(role)),
                    ("differences", delta),
                ]));
            }
        } else if left == "rejected" && right == "rejected" {
            let disagree = role == "debug"
                && (candidate.get("returncode"), candidate.get("stderr_sha256"))
                    != (other.get("returncode"), other.get("stderr_sha256"));
            if disagree {
                findings.push(Value::object([
                    ("kind", Value::str("debug-rejection-disagreement")),
                    ("role", Value::str(role)),
                ]));
            }
            comparisons.push(Value::object([
                ("role", Value::str(role)),
                ("status", Value::str("both-rejected-ungraded")),
            ]));
        } else {
            comparisons.push(Value::object([
                ("role", Value::str(role)),
                ("status", Value::str("not-equivalent")),
                ("candidate", Value::str(left)),
                ("other", Value::str(right)),
            ]));
            if matches!(left, "ok" | "rejected") && matches!(right, "ok" | "rejected") {
                findings.push(Value::object([
                    ("kind", Value::str("acceptance-disagreement")),
                    ("role", Value::str(role)),
                ]));
            }
        }
    }
    Ok(Value::object([
        ("comparisons", Value::Array(comparisons)),
        ("findings", Value::Array(findings)),
        ("qualification", Value::Bool(false)),
    ]))
}

pub struct Campaign {
    pub manifest: PathBuf,
    pub output: PathBuf,
    pub rounds: usize,
    pub seed: u64,
    pub maximum_input_bytes: usize,
    pub maximum_output_bytes: usize,
    pub timeout_seconds: f64,
}

fn label(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn sha(path: &Path) -> R<String> {
    Ok(digest_bytes(
        &fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
}

fn write(path: &Path, value: &Value) -> R<()> {
    let temporary = path.with_extension("tmp");
    crate::corpus::write_json(&temporary, value)?;
    fs::rename(&temporary, path).map_err(|e| e.to_string())
}

fn input_identity(path: &Path, expected: &str, expected_bytes: usize, maximum: usize) -> Value {
    match fs::metadata(path) {
        Ok(meta) if meta.len() as usize > maximum => Value::object([
            ("matches", Value::Bool(false)),
            ("bytes", Value::int(meta.len() as i64)),
            ("sha256", Value::Null),
            ("error", Value::str("input-size-limit")),
        ]),
        Ok(meta) => match sha(path) {
            Ok(actual) => Value::object([
                (
                    "matches",
                    Value::Bool(actual == expected && meta.len() as usize == expected_bytes),
                ),
                ("bytes", Value::int(meta.len() as i64)),
                ("sha256", Value::str(actual)),
            ]),
            Err(_) => Value::object([
                ("matches", Value::Bool(false)),
                ("error", Value::str("OSError")),
                ("sha256", Value::Null),
            ]),
        },
        Err(error) => {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                "FileNotFoundError"
            } else {
                "OSError"
            };
            Value::object([
                ("matches", Value::Bool(false)),
                ("error", Value::str(kind)),
                ("sha256", Value::Null),
            ])
        }
    }
}

fn count(counts: &mut Vec<(String, Value)>, key: &str) {
    match counts.iter_mut().find(|(k, _)| k == key) {
        Some((_, Value::Number(n))) => {
            *n = match Value::int(n.as_i128().unwrap_or(0) + 1) {
                Value::Number(m) => m,
                _ => unreachable!(),
            }
        }
        _ => counts.push((key.to_owned(), Value::int(1))),
    }
}

/// Run a pinned campaign; returns the exit code (1 when anything was found or
/// an input changed under a worker).
pub fn campaign(root: &Path, args: &Campaign) -> R<i32> {
    if !(1..=10000).contains(&args.rounds) || !(1..=64 << 20).contains(&args.maximum_input_bytes) {
        return Err("invalid finite mutation bounds".into());
    }
    if !((1..=64 << 20).contains(&args.maximum_output_bytes)
        && args.timeout_seconds > 0.0
        && args.timeout_seconds <= 60.0)
    {
        return Err("invalid finite observation bounds".into());
    }
    let research = root.join(".research");
    let output = std::path::absolute(&args.output).map_err(|e| e.to_string())?;
    if !output.starts_with(&research) || output.exists() {
        return Err("output must be a new directory under .research".into());
    }
    let resolve = |p: &str| -> PathBuf {
        if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            root.join(p)
        }
    };
    let manifest_bytes =
        fs::read(&args.manifest).map_err(|e| format!("{}: {e}", args.manifest.display()))?;
    let manifest_digest = digest_bytes(&manifest_bytes);
    let manifest = strict_json(&manifest_bytes)?;
    if py_int(manifest.get("schema")) != Some(1) {
        return Err("unknown mutation manifest schema".into());
    }
    let (Some(Value::Object(workers)), Some(Value::Array(operations)), Some(Value::Array(seeds))) = (
        manifest.get("workers"),
        manifest.get("operations"),
        manifest.get("seeds"),
    ) else {
        return Err("workers, operations and seeds must be nonempty".into());
    };
    if workers.is_empty() || operations.is_empty() || seeds.is_empty() {
        return Err("workers, operations and seeds must be nonempty".into());
    }
    let unique = |items: &[Value]| {
        let mut ids: Vec<&str> = items.iter().filter_map(|i| s(i, "id")).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len() == items.len()
    };
    if !unique(seeds) || !unique(operations) {
        return Err("duplicate seed or operation identities".into());
    }
    if workers.len() > 8 || operations.len() > 16 || !workers.iter().all(|(role, _)| label(role)) {
        return Err("bounded worker/operation counts and safe role labels are required".into());
    }
    if !operations.iter().all(|o| s(o, "id").is_some_and(label)) {
        return Err("operation IDs must be simple bounded labels".into());
    }
    let worker_paths: Vec<(String, PathBuf)> = workers
        .iter()
        .map(|(role, w)| (role.clone(), resolve(s(w, "path").unwrap_or_default())))
        .collect();
    for ((role, path), (_, worker)) in worker_paths.iter().zip(workers) {
        if Some(sha(path)?.as_str()) != s(worker, "sha256") {
            return Err(format!("worker hash mismatch: {role}"));
        }
    }
    let sources = match manifest.get("source_files") {
        Some(Value::Object(m)) if !m.is_empty() => m.clone(),
        _ => return Err("explicit source/input-tool provenance hashes are required".into()),
    };
    for (path, digest) in &sources {
        if Some(sha(&resolve(path))?.as_str()) != digest.as_str() {
            return Err(format!("source provenance mismatch: {path}"));
        }
    }
    let mut seed_bytes = Vec::new();
    for row in seeds {
        let id = s(row, "id").unwrap_or_default().to_owned();
        let data =
            fs::read(resolve(s(row, "path").unwrap_or_default())).map_err(|e| e.to_string())?;
        if Some(digest_bytes(&data).as_str()) != s(row, "sha256")
            || data.len() > args.maximum_input_bytes
        {
            return Err(format!("seed hash or input-size mismatch: {id}"));
        }
        if !row.get("rights").is_some_and(Value::truthy) {
            return Err(format!("seed rights/provenance must be recorded: {id}"));
        }
        let expected = row
            .get("baseline_statuses")
            .cloned()
            .unwrap_or(Value::Object(vec![]));
        let Value::Object(declared) = &expected else {
            return Err("every seed must declare each baseline operation".into());
        };
        let mut declared_ids: Vec<&str> = declared.iter().map(|(k, _)| k.as_str()).collect();
        let mut operation_ids: Vec<&str> = operations.iter().filter_map(|o| s(o, "id")).collect();
        declared_ids.sort_unstable();
        operation_ids.sort_unstable();
        if declared_ids != operation_ids {
            return Err("every seed must declare each baseline operation".into());
        }
        for operation in operations {
            let statuses = expected
                .get(s(operation, "id").unwrap_or_default())
                .cloned()
                .unwrap_or(Value::Null);
            let roles = operation.get("roles").cloned().unwrap_or(Value::Null);
            let keys = |v: &Value| match v {
                Value::Object(m) => sorted_keys(m)
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                _ => vec![],
            };
            let valid = keys(&statuses) == keys(&roles)
                && matches!(&statuses, Value::Object(m) if m.iter().all(|(_, v)| matches!(v.as_str(), Some("ok" | "rejected"))));
            if !valid {
                return Err("every baseline worker outcome must be declared".into());
            }
        }
        seed_bytes.push((id, data));
    }
    for operation in operations {
        let Some(Value::Object(roles)) = operation.get("roles") else {
            return Err("operation needs candidate and at least one pinned comparator".into());
        };
        if !roles.iter().any(|(r, _)| r == "candidate")
            || roles.len() < 2
            || !roles
                .iter()
                .all(|(r, _)| workers.iter().any(|(w, _)| w == r))
        {
            return Err("operation needs candidate and at least one pinned comparator".into());
        }
        for (_, command) in roles {
            let ok = command.as_array().is_some_and(|c| {
                c.iter().all(|v| v.as_str().is_some())
                    && c.iter().filter(|v| v.as_str() == Some("{input}")).count() == 1
            });
            if !ok {
                return Err("worker arguments need exactly one literal {input} slot".into());
            }
        }
    }
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    fs::write(output.join("manifest.json"), &manifest_bytes).map_err(|e| e.to_string())?;
    let controller_files = [
        "xtask/src/fuzz.rs",
        "xtask/src/json.rs",
        "xtask/src/harness/diff.rs",
        "xtask/src/pyrandom.rs",
    ];
    let mut controller = Vec::new();
    for relative in controller_files {
        let digest = sha(&root.join(relative))?;
        let destination = output.join("controller-source").join(relative);
        fs::create_dir_all(destination.parent().expect("parent")).map_err(|e| e.to_string())?;
        fs::copy(root.join(relative), &destination).map_err(|e| e.to_string())?;
        if sha(&destination)? != digest {
            return Err(format!(
                "controller changed during preservation: {relative}"
            ));
        }
        controller.push((relative.to_owned(), Value::str(digest)));
    }
    let rustc = Command::new("rustc")
        .arg("-V")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let identity = Value::object([
        ("manifest_sha256", Value::str(&manifest_digest)),
        ("controller_sha256", Value::Object(controller.clone())),
        ("workers", Value::Object(workers.clone())),
        ("source_files", Value::Object(sources.clone())),
        ("rustc", Value::str(rustc)),
        (
            "platform",
            Value::str(crate::harness::report::host_target()),
        ),
        ("machine", Value::str(crate::harness::linux::machine())),
        (
            "parameters",
            Value::object([
                ("manifest", Value::str(args.manifest.display().to_string())),
                ("output", Value::str(output.display().to_string())),
                ("rounds", Value::int(args.rounds as i64)),
                ("seed", Value::int(args.seed as i64)),
                (
                    "maximum_input_bytes",
                    Value::int(args.maximum_input_bytes as i64),
                ),
                (
                    "maximum_output_bytes",
                    Value::int(args.maximum_output_bytes as i64),
                ),
                ("timeout_seconds", Value::float(args.timeout_seconds)),
            ]),
        ),
        ("qualification", Value::Bool(false)),
        ("time_or_memory_measurement", Value::Bool(false)),
    ]);
    write(&output.join("identity.json"), &identity)?;
    if sha(&args.manifest)? != manifest_digest {
        return Err("campaign manifest changed after its initial read".into());
    }
    let mut rng = PyRandom::new(args.seed);
    let (mut counts, mut finding_counts) = (Vec::new(), Vec::new());
    let (mut cases, mut finding_total, mut executions) = (0usize, 0usize, 0usize);
    let mut unchanged = true;
    let index = output.join("findings.jsonl");
    fs::write(&index, b"").map_err(|e| e.to_string())?;
    let timeout = Duration::from_secs_f64(args.timeout_seconds);
    for (row, (id, original)) in seeds.iter().zip(&seed_bytes) {
        for round in 0..=args.rounds {
            let (data, recipe) = if round == 0 {
                (original.clone(), Value::Null)
            } else {
                mutation(original, &mut rng, args.maximum_input_bytes, 4)?
            };
            let case = output.join(format!("case-{cases:06}"));
            fs::create_dir(&case).map_err(|e| e.to_string())?;
            let path = case.join("input.bin");
            fs::write(&path, &data).map_err(|e| e.to_string())?;
            fs::write(case.join("initial-input.bin"), &data).map_err(|e| e.to_string())?;
            let input_sha = digest_bytes(&data);
            let mut entries = Vec::new();
            for operation in operations {
                let op = s(operation, "id").unwrap_or_default();
                let directory = case.join(op);
                fs::create_dir(&directory).map_err(|e| e.to_string())?;
                let Some(Value::Object(roles)) = operation.get("roles") else {
                    unreachable!()
                };
                let mut observed: Vec<(String, Value)> = Vec::new();
                for (role, arguments) in roles {
                    let worker = &worker_paths
                        .iter()
                        .find(|(r, _)| r == role)
                        .expect("declared worker")
                        .1;
                    let mut command: Vec<OsString> = vec![worker.as_os_str().to_owned()];
                    for a in arguments.as_array().unwrap_or_default() {
                        let a = a.as_str().unwrap_or_default();
                        command.push(if a == "{input}" {
                            path.as_os_str().to_owned()
                        } else {
                            a.into()
                        });
                    }
                    let before =
                        input_identity(&path, &input_sha, data.len(), args.maximum_input_bytes);
                    let matches = |v: &Value| v.get("matches") == Some(&Value::Bool(true));
                    let (mut record, stdout, stderr) = if matches(&before) {
                        executions += 1;
                        observe(&command, timeout, args.maximum_output_bytes)?
                    } else {
                        let record = Value::object([
                            ("status", Value::str("input-identity-changed")),
                            ("returncode", Value::Null),
                            (
                                "command",
                                Value::strings(
                                    command.iter().map(|c| c.to_string_lossy().into_owned()),
                                ),
                            ),
                            ("stdout_sha256", Value::str(digest_bytes(b""))),
                            ("stderr_sha256", Value::str(digest_bytes(b""))),
                            ("skipped", Value::Bool(true)),
                        ]);
                        (record, vec![], vec![])
                    };
                    let after =
                        input_identity(&path, &input_sha, data.len(), args.maximum_input_bytes);
                    let Value::Object(fields) = &mut record else {
                        unreachable!()
                    };
                    fields.push(("input_before".into(), before.clone()));
                    fields.push(("input_after".into(), after.clone()));
                    if !matches(&before) || !matches(&after) {
                        unchanged = false;
                        let status = fields[0].1.clone();
                        if status.as_str() != Some("input-identity-changed") {
                            fields.push(("worker_status".into(), status));
                            fields[0].1 = Value::str("input-identity-changed");
                        }
                    }
                    fs::write(directory.join(format!("{role}.stdout")), &stdout)
                        .map_err(|e| e.to_string())?;
                    fs::write(directory.join(format!("{role}.stderr")), &stderr)
                        .map_err(|e| e.to_string())?;
                    count(
                        &mut counts,
                        &format!("{role}:{}", s(&record, "status").unwrap_or_default()),
                    );
                    observed.push((role.clone(), record));
                }
                let mut comparison = compare(&observed)?;
                if round == 0
                    && let Some(Value::Object(expected)) =
                        row.get("baseline_statuses").and_then(|b| b.get(op))
                    && let Some((_, Value::Array(findings))) = match &mut comparison {
                        Value::Object(m) => m.iter_mut().find(|(k, _)| k == "findings"),
                        _ => None,
                    }
                {
                    for (role, status) in expected {
                        let actual = observed
                            .iter()
                            .find(|(r, _)| r == role)
                            .and_then(|(_, v)| s(v, "status"))
                            .unwrap_or_default();
                        if Some(actual) != status.as_str() {
                            findings.push(Value::object([
                                ("kind", Value::str("baseline-status-changed")),
                                ("role", Value::str(role)),
                                ("expected", status.clone()),
                                ("actual", Value::str(actual)),
                            ]));
                        }
                    }
                }
                // Raw streams stay authoritative; parsed values are not retained.
                for (_, record) in &mut observed {
                    if let Value::Object(fields) = record {
                        fields.retain(|(k, _)| k != "data");
                    }
                }
                let mut entry = vec![
                    ("id".to_owned(), Value::str(op)),
                    ("workers".to_owned(), Value::Object(observed)),
                ];
                if let Value::Object(fields) = &comparison {
                    entry.extend(fields.iter().cloned());
                }
                for finding in comparison
                    .get("findings")
                    .and_then(Value::as_array)
                    .unwrap_or_default()
                {
                    count(&mut finding_counts, s(finding, "kind").unwrap_or_default());
                    finding_total += 1;
                    let mut line = vec![
                        (
                            "case".to_owned(),
                            Value::str(case.file_name().unwrap().to_string_lossy()),
                        ),
                        ("operation".to_owned(), Value::str(op)),
                    ];
                    if let Value::Object(fields) = finding {
                        line.extend(fields.iter().cloned());
                    }
                    let mut stream = fs::OpenOptions::new()
                        .append(true)
                        .open(&index)
                        .map_err(|e| e.to_string())?;
                    writeln!(stream, "{}", json::to_python(&Value::Object(line), None))
                        .map_err(|e| e.to_string())?;
                }
                entries.push(Value::Object(entry));
            }
            let final_identity =
                input_identity(&path, &input_sha, data.len(), args.maximum_input_bytes);
            unchanged &= final_identity.get("matches") == Some(&Value::Bool(true));
            let result = Value::object([
                ("ordinal", Value::int(cases as i64)),
                ("seed_id", Value::str(id)),
                (
                    "seed_sha256",
                    row.get("sha256").cloned().unwrap_or(Value::Null),
                ),
                ("rights", row.get("rights").cloned().unwrap_or(Value::Null)),
                ("round", Value::int(round as i64)),
                ("input_sha256", Value::str(&input_sha)),
                ("recipe", recipe),
                ("operations", Value::Array(entries)),
                ("qualification", Value::Bool(false)),
                ("final_input_identity", final_identity),
            ]);
            write(&case.join("observation.json"), &result)?;
            cases += 1;
            write(
                &output.join("progress.json"),
                &Value::object([
                    ("completed_cases", Value::int(cases as i64)),
                    ("worker_outcomes", Value::Object(counts.clone())),
                    ("findings_count", Value::int(finding_total as i64)),
                    ("findings_by_kind", Value::Object(finding_counts.clone())),
                    ("collection_status", Value::str("partial")),
                    ("qualification", Value::Bool(false)),
                ]),
            )?;
        }
        println!(
            "{} {} original/mutated inputs retained",
            json::to_python(&Value::str(id), None),
            args.rounds + 1
        );
    }
    for ((role, path), (_, worker)) in worker_paths.iter().zip(workers) {
        if Some(sha(path)?.as_str()) != s(worker, "sha256") {
            return Err(format!("worker changed during campaign: {role}"));
        }
    }
    for (path, digest) in &sources {
        if Some(sha(&resolve(path))?.as_str()) != digest.as_str() {
            return Err(format!("source provenance changed during campaign: {path}"));
        }
    }
    for row in seeds {
        if Some(sha(&resolve(s(row, "path").unwrap_or_default()))?.as_str()) != s(row, "sha256") {
            return Err(format!(
                "seed changed during campaign: {}",
                s(row, "id").unwrap_or_default()
            ));
        }
    }
    for (relative, digest) in &controller {
        if Some(sha(&root.join(relative))?.as_str()) != digest.as_str() {
            return Err(format!("controller changed during campaign: {relative}"));
        }
    }
    if sha(&args.manifest)? != manifest_digest {
        return Err("campaign manifest changed during execution".into());
    }
    let report = Value::object([
        (
            "collection_status",
            Value::str(if unchanged {
                "complete"
            } else {
                "input-identity-failed"
            }),
        ),
        ("seeds", Value::int(seeds.len() as i64)),
        ("operations", Value::int(operations.len() as i64)),
        ("cases", Value::int(cases as i64)),
        ("worker_outcomes", Value::Object(counts)),
        ("findings_count", Value::int(finding_total as i64)),
        ("findings_by_kind", Value::Object(finding_counts)),
        ("worker_executions", Value::int(executions as i64)),
        ("case_inputs_unchanged", Value::Bool(unchanged)),
        (
            "findings_index",
            Value::object([
                ("path", Value::str("findings.jsonl")),
                ("sha256", Value::str(sha(&index)?)),
            ]),
        ),
        ("source_workers_and_seeds_unchanged", Value::Bool(true)),
        ("qualification", Value::Bool(false)),
        ("time_or_memory_measurement", Value::Bool(false)),
        ("rejections_are_ungraded", Value::Bool(true)),
        (
            "scope",
            Value::str(
                "wire mutation/differential observations; stateful scheduling remains separate",
            ),
        ),
    ]);
    write(&output.join("report.json"), &report)?;
    println!("{}", json::to_python(&report, Some(2)));
    Ok(i32::from(finding_total > 0 || !unchanged))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutations_replay_and_reject_tampered_recipes() {
        let seed: Vec<u8> = (0..200u8).collect();
        let mut rng = PyRandom::new(280);
        for _ in 0..50 {
            let (data, recipe) = mutation(&seed, &mut rng, 4096, 4).unwrap();
            assert_ne!(data, seed);
            assert_eq!(replay(&seed, &recipe, 4096).unwrap(), data);
            let mut tampered = recipe.clone();
            if let Value::Object(m) = &mut tampered {
                m.retain(|(k, _)| k != "result_sha256");
                m.push(("result_sha256".into(), Value::str("0".repeat(64))));
            }
            assert!(replay(&seed, &tampered, 4096).is_err());
        }
        let (empty, _) = mutation(&[], &mut rng, 16, 1).unwrap();
        assert!(!empty.is_empty());
        assert!(
            mutation(&seed, &mut rng, 10, 4).is_err(),
            "the seed exceeds the bound"
        );
    }

    #[test]
    fn mutations_match_the_recorded_python_stream() {
        // The first recipe Python's generator writes for this seed and RNG state.
        let seed = b"0123456789abcdef".to_vec();
        let mut rng = PyRandom::new(7);
        let (_, recipe) = mutation(&seed, &mut rng, 64, 4).unwrap();
        assert_eq!(json::canonical(recipe.get("edits").unwrap()), PYTHON_EDITS);
    }

    const PYTHON_EDITS: &str = r#"[{"insert_hex":"","offset":12,"remove":1},{"insert_hex":"67","offset":13,"remove":1},{"insert_hex":"","offset":9,"remove":6}]"#;

    #[test]
    fn strict_json_bounds_documents() {
        assert!(strict_json(b"{\"a\": [1, 2.50]}").is_ok());
        assert!(
            strict_json(
                &[b'['; 300]
                    .iter()
                    .chain([b']'; 300].iter())
                    .copied()
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
        assert!(strict_json(b"{\"a\": 1, \"a\": 1}").is_err());
        assert!(strict_json(b"\xff").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn observations_classify_bounded_runs() {
        let sh = |script: &str| vec![OsString::from("sh"), "-c".into(), script.into()];
        let (ok, stdout, _) =
            observe(&sh("printf '{\"a\": 1.50}'"), Duration::from_secs(5), 1024).unwrap();
        assert_eq!(s(&ok, "status"), Some("ok"));
        assert_eq!(stdout, b"{\"a\": 1.50}");
        assert_eq!(
            s(
                &observe(&sh("exit 3"), Duration::from_secs(5), 1024)
                    .unwrap()
                    .0,
                "status"
            ),
            Some("rejected")
        );
        assert_eq!(
            s(
                &observe(&sh("exit 101"), Duration::from_secs(5), 1024)
                    .unwrap()
                    .0,
                "status"
            ),
            Some("crash")
        );
        assert_eq!(
            s(
                &observe(&sh("kill -SEGV $$"), Duration::from_secs(5), 1024)
                    .unwrap()
                    .0,
                "status"
            ),
            Some("crash")
        );
        assert_eq!(
            s(
                &observe(&sh("echo nope"), Duration::from_secs(5), 1024)
                    .unwrap()
                    .0,
                "status"
            ),
            Some("invalid-json")
        );
        let (slow, _, _) = observe(&sh("sleep 10"), Duration::from_millis(200), 1024).unwrap();
        assert_eq!(s(&slow, "status"), Some("timeout"));
        let (big, stdout, _) = observe(
            &sh("head -c 100000 /dev/zero"),
            Duration::from_secs(5),
            1000,
        )
        .unwrap();
        assert_eq!(
            (s(&big, "status"), stdout.len()),
            (Some("output-limit"), 1000)
        );
    }

    #[test]
    fn comparisons_keep_rejections_ungraded_and_decimals_exact() {
        let ok = |data: &str| {
            Value::object([
                ("status", Value::str("ok")),
                ("data", json::parse_strict(data).unwrap()),
            ])
        };
        let result = compare(&[
            ("candidate".into(), ok(r#"{"v": 1.50}"#)),
            ("reference".into(), ok(r#"{"v": 1.5}"#)),
        ])
        .unwrap();
        assert_eq!(result.get("findings"), Some(&Value::Array(vec![])));
        let result = compare(&[
            ("candidate".into(), ok(r#"{"v": 1.50}"#)),
            ("reference".into(), ok(r#"{"v": 1.25}"#)),
        ])
        .unwrap();
        let shown = json::to_python(result.get("findings").unwrap(), None);
        assert!(shown.contains(r#"{"json_decimal": "1.50"}"#), "{shown}");
        let rejected = Value::object([
            ("status", Value::str("rejected")),
            ("returncode", Value::int(2)),
            ("stderr_sha256", Value::str("a")),
        ]);
        let both = compare(&[
            ("candidate".into(), rejected.clone()),
            ("reference".into(), rejected.clone()),
        ])
        .unwrap();
        assert_eq!(both.get("findings"), Some(&Value::Array(vec![])));
        let mixed = compare(&[
            ("candidate".into(), ok("{}")),
            ("reference".into(), rejected),
        ])
        .unwrap();
        assert!(json::to_python(&mixed, None).contains("acceptance-disagreement"));
        assert!(compare(&[("candidate".into(), ok("{}"))]).is_err());
    }

    #[test]
    fn replay_rejects_edits_outside_the_current_input() {
        let seed = b"abcdef".to_vec();
        let recipe = |edits: &str, result: &[u8]| {
            json::parse(&format!(
                r#"{{"schema": 1, "seed_sha256": "{}", "edits": {edits}, "result_sha256": "{}"}}"#,
                digest_bytes(&seed),
                digest_bytes(result)
            ))
            .unwrap()
        };
        assert_eq!(
            replay(
                &seed,
                &recipe(
                    r#"[{"offset": 1, "remove": 2, "insert_hex": "ff"}]"#,
                    b"a\xffdef"
                ),
                16
            )
            .unwrap(),
            b"a\xffdef"
        );
        for edits in [
            r#"[{"offset": 7, "remove": 0, "insert_hex": ""}]"#,
            r#"[{"offset": -1, "remove": 0, "insert_hex": ""}]"#,
            r#"[{"offset": 2, "remove": 5, "insert_hex": ""}]"#,
            r#"[{"offset": 0, "remove": 0, "insert_hex": "zz"}]"#,
            r#"[{"offset": 0, "remove": 0, "insert_hex": "f"}]"#,
            r#"[{"offset": true, "remove": 0, "insert_hex": ""}]"#,
            r#"[{"offset": 0, "remove": 0, "insert_hex": "", "extra": 1}]"#,
            r"[]",
        ] {
            assert!(replay(&seed, &recipe(edits, &seed), 16).is_err(), "{edits}");
        }
        assert!(
            replay(
                &seed,
                &recipe(
                    r#"[{"offset": 0, "remove": 0, "insert_hex": "00000000000000000000"}]"#,
                    b""
                ),
                8
            )
            .is_err(),
            "over the bound"
        );
    }

    #[test]
    fn booleans_never_equal_numbers_and_long_decimals_stay_exact() {
        let ok = |data: &str| {
            Value::object([
                ("status", Value::str("ok")),
                ("data", json::parse_strict(data).unwrap()),
            ])
        };
        let result = compare(&[
            ("candidate".into(), ok(r#"{"a": true}"#)),
            ("reference".into(), ok(r#"{"a": 1}"#)),
        ])
        .unwrap();
        assert!(result.get("findings").is_some_and(Value::truthy));
        let long = format!("0.{}", "1".repeat(300));
        let result = compare(&[
            ("candidate".into(), ok(&format!(r"[{long}]"))),
            ("reference".into(), ok("[0.5]")),
        ])
        .unwrap();
        assert!(
            json::to_python(&result, None).contains(&long),
            "a long decimal is kept, not elided"
        );
        let shown = compare(&[
            ("candidate".into(), ok(&format!(r#""{}""#, "x".repeat(300)))),
            ("reference".into(), ok(r#""y""#)),
        ])
        .unwrap();
        assert!(json::to_python(&shown, None).contains("<large value>"));
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_worker_leaves_no_descendants_behind() {
        let dir = crate::walk::scratch("fuzz-group");
        let marker = dir.join("child.pid");
        let script = format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > {}; printf '{{}}'",
            marker.display()
        );
        let (result, _, _) = observe(
            &[OsString::from("sh"), "-c".into(), script.into()],
            Duration::from_secs(5),
            1024,
        )
        .unwrap();
        assert_eq!(s(&result, "status"), Some("ok"));
        let pid = fs::read_to_string(&marker).unwrap().trim().to_owned();
        std::thread::sleep(Duration::from_millis(100));
        let alive = Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the background child was not terminated");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn labels_are_bounded_lowercase_identifiers() {
        assert!(label("candidate") && label("debug-2") && label("a_b"));
        for bad in ["", "Candidate", "../x", "a/b", "1abc", &"a".repeat(65)] {
            assert!(!label(bad), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_campaign_keeps_raw_streams_and_flags_input_tampering() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::walk::scratch("fuzz-campaign");
        fs::create_dir_all(root.join(".research")).unwrap();
        fs::create_dir_all(root.join("xtask/src/harness")).unwrap();
        for file in [
            "xtask/src/fuzz.rs",
            "xtask/src/json.rs",
            "xtask/src/harness/diff.rs",
            "xtask/src/pyrandom.rs",
        ] {
            fs::write(root.join(file), file).unwrap();
        }
        let worker = |name: &str, body: &str| {
            let path = root.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let candidate = worker("candidate", "printf '{\"n\": 1.50}'");
        let reference = worker("reference", "printf '{\"n\": 1.5}'");
        let tamper = worker("tamper", "printf x >> \"$2\"; printf '{\"n\": 1.5}'");
        fs::write(root.join("seed.bin"), b"seed bytes").unwrap();
        let sha = |p: &Path| digest_bytes(&fs::read(p).unwrap());
        let manifest = |comparator: &Path| {
            format!(
                r#"{{"schema": 1, "workers": {{"candidate": {{"path": "{}", "sha256": "{}"}}, "reference": {{"path": "{}", "sha256": "{}"}}}},
                "source_files": {{"seed.bin": "{}"}},
                "operations": [{{"id": "trace", "roles": {{"candidate": ["trace", "{{input}}"], "reference": ["trace", "{{input}}"]}}}}],
                "seeds": [{{"id": "seed", "path": "seed.bin", "sha256": "{}", "rights": "authored-synthetic", "baseline_statuses": {{"trace": {{"candidate": "ok", "reference": "ok"}}}}}}]}}"#,
                candidate.display(),
                sha(&candidate),
                comparator.display(),
                sha(comparator),
                sha(&root.join("seed.bin")),
                sha(&root.join("seed.bin"))
            )
        };
        let run = |name: &str, comparator: &Path| {
            fs::write(root.join(format!("{name}.json")), manifest(comparator)).unwrap();
            let args = Campaign {
                manifest: root.join(format!("{name}.json")),
                output: root.join(".research").join(name),
                rounds: 3,
                seed: 280,
                maximum_input_bytes: 1024,
                maximum_output_bytes: 1024,
                timeout_seconds: 5.0,
            };
            (
                campaign(&root, &args).unwrap(),
                crate::checks::read_json(&root.join(".research").join(name).join("report.json"))
                    .unwrap(),
            )
        };
        let (code, report) = run("agree", &reference);
        assert_eq!(code, 0);
        assert_eq!(report.get("cases"), Some(&Value::int(4)));
        assert_eq!(
            report.get("collection_status").and_then(Value::as_str),
            Some("complete")
        );
        assert!(
            root.join(".research/agree/case-000003/trace/candidate.stdout")
                .is_file()
        );
        let (code, report) = run("tamper", &tamper);
        assert_eq!(code, 1);
        assert_eq!(
            report.get("collection_status").and_then(Value::as_str),
            Some("input-identity-failed")
        );
        let outside = Campaign {
            manifest: root.join("agree.json"),
            output: root.join("elsewhere"),
            rounds: 1,
            seed: 1,
            maximum_input_bytes: 1024,
            maximum_output_bytes: 1024,
            timeout_seconds: 1.0,
        };
        assert!(
            campaign(&root, &outside).is_err(),
            "output must be under .research"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
