//! Linux memory instrumentation evidence.
//!
//! The LD_PRELOAD heap observer only counts what it intercepts, so a Linux
//! measurement is admitted only with proof that nothing else could allocate in
//! the measured window: the resolved ELF dependency closure of every selected
//! binary is sealed by content hash, every allocator import is covered by a
//! hook or by the active-window guard, and a native calibration (positive and
//! negative controls per guarded entry point) certifies the guard itself. An
//! optional kernel module tightens the resident-set upper bound and carries its
//! own reviewed proof. Certificates are recomputed from their raw records
//! wherever they are used.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::diff::equal;
use crate::json::{self, Value};
use crate::sha256;

/// The entry points the observer hooks, in its receipts' `symbol_order`.
/// Receipts from before the two string hooks list the first six.
pub const ALLOCATORS: [&str; 8] = [
    "malloc",
    "calloc",
    "realloc",
    "free",
    "posix_memalign",
    "aligned_alloc",
    "strdup",
    "strndup",
];
pub const GUARD_ORDER: [&str; 7] = [
    "mmap",
    "mmap64",
    "munmap",
    "pthread_create",
    "dlopen",
    "dlmopen",
    "dlclose",
];
pub const GUARD_POLICY: &str = "invalidate-any-active-attempt-v1";
pub const SCOPE_ERROR_BIT: i128 = 1 << 12;
pub const TLS_IMPORTS: [&str; 3] = [
    "_dl_allocate_tls",
    "_dl_allocate_tls_init",
    "_dl_deallocate_tls",
];
/// Negative controls: probe argument, guarded entry index, whether the call succeeds.
pub const GUARD_CASES: [(&str, usize, bool); 12] = [
    ("guard-mmap", 0, true),
    ("guard-mmap64", 1, true),
    ("guard-munmap", 2, true),
    ("guard-mmap-failure", 0, false),
    ("guard-mmap64-failure", 1, false),
    ("guard-munmap-failure", 2, false),
    ("guard-pthread-create", 3, true),
    ("guard-dlopen", 4, true),
    ("guard-dlmopen", 5, true),
    ("guard-dlclose", 6, true),
    ("guard-dlopen-failure", 4, false),
    ("guard-dlmopen-failure", 5, false),
];
pub const KERNEL_RSS_BASIS: &str = "min-kernel-positive-deltas-lifetime-VmPeak";
const KERNEL_RSS_ROLES: [&str; 8] = [
    "module_source",
    "module_api",
    "observer_source",
    "observer_api",
    "probe_source",
    "module_binary",
    "observer_binary",
    "probe_binary",
];
const KERNEL_NOTE: &str = "/sys/module/triergon_rss_bound/notes/.note.gnu.build-id";
const KERNEL_RUNTIME_FILES: [&str; 3] = [
    "/sys/kernel/notes",
    "/proc/sys/kernel/random/boot_id",
    KERNEL_NOTE,
];

type R<T> = Result<T, String>;
type Members = Vec<(String, Value)>;

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A Python `int` (not a bool, not a float).
pub fn py_int(value: Option<&Value>) -> Option<i128> {
    match value? {
        Value::Number(n) if n.is_integer() => n.as_i128(),
        _ => None,
    }
}

fn is_true(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
}

fn is_false(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(false)))
}

fn ints(value: Option<&Value>, len: usize) -> Option<Vec<i128>> {
    let items = value?.as_array()?;
    if items.len() != len {
        return None;
    }
    items.iter().map(|v| py_int(Some(v))).collect()
}

/// `canonical_digest`: SHA-256 of the sorted, compact JSON form.
pub fn digest(value: &Value) -> String {
    sha256::hex(json::canonical(value).as_bytes())
}

pub fn file_sha(path: &Path) -> R<String> {
    Ok(sha256::hex(
        &fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
}

fn strs(items: impl IntoIterator<Item = String>) -> Value {
    Value::strings(items)
}

// ---------------------------------------------------------------------------
// Allocator imports.

fn allocation_name(name: &str) -> bool {
    let base = name.strip_prefix("__").unwrap_or(name);
    name.contains("alloc")
        || name.ends_with("free")
        || name.contains("strdup")
        || name.contains("strndup")
        || name.contains("asprintf")
        || matches!(
            base,
            "mmap" | "mmap64" | "munmap" | "mremap" | "brk" | "sbrk"
        )
        || ["_Zn", "_ZN", "_Zd", "_ZD"]
            .iter()
            .any(|p| name.starts_with(p))
}

/// C++'s replaceable allocation functions: every form of operator new and
/// delete (`_Znw`, `_Zna`, `_Zdl`, `_Zda`).
fn replaceable(name: &str) -> bool {
    ["_Znw", "_Zna", "_Zdl", "_Zda"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Names a GNU nm listing imports only as weak (`w` or `v`) references.
fn weak_only(listing: &str) -> BTreeSet<String> {
    let (mut weak, mut strong) = (BTreeSet::new(), BTreeSet::new());
    for fields in listing
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
    {
        if let [symbol, kind, ..] = fields[..] {
            let name = symbol.split('@').next().unwrap_or_default().to_owned();
            if matches!(kind, "w" | "v") {
                weak.insert(name);
            } else {
                strong.insert(name);
            }
        }
    }
    weak.difference(&strong).cloned().collect()
}

/// Raw undefined symbols and the allocator entry points among them; a name not
/// in `ALLOCATORS` is uncovered until something else proves it harmless.
pub fn allocator_symbols(listing: &str, linux: bool) -> R<Value> {
    let (raw, allocation): (Vec<String>, BTreeSet<String>) = if linux {
        let mut raw = Vec::new();
        for line in listing.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.is_empty() {
                continue;
            }
            if fields.len() < 2 || !matches!(fields[1], "U" | "w" | "v") {
                return Err(format!("unparsed GNU nm undefined-symbol record: {line}"));
            }
            raw.push(fields[0].to_owned());
        }
        let names: BTreeSet<String> = raw
            .iter()
            .map(|n| n.split('@').next().unwrap_or_default().to_owned())
            .collect();
        let mut allocation: BTreeSet<String> = names
            .iter()
            .filter(|n| *n != "malloc_usable_size" && allocation_name(n))
            .cloned()
            .collect();
        let extra = GUARD_ORDER.iter().chain(&[
            "clone",
            "clone3",
            "__clone",
            "thrd_create",
            "fork",
            "vfork",
            "execve",
            "execveat",
        ]);
        allocation.extend(
            extra
                .filter(|n| names.contains(**n))
                .map(ToString::to_string),
        );
        (raw, allocation)
    } else {
        let raw: Vec<String> = listing
            .lines()
            .map(str::trim)
            .filter(|l| {
                l.starts_with('_')
                    && l.len() > 1
                    && l.chars().all(|c| c.is_alphanumeric() || c == '_')
            })
            .map(str::to_owned)
            .collect();
        let allocation = raw
            .iter()
            .filter(|n| {
                n.contains("alloc")
                    || n.ends_with("free")
                    || n.contains("strdup")
                    || n.contains("strndup")
                    || n.contains("asprintf")
            })
            .map(|n| n.trim_start_matches('_').to_owned())
            .collect();
        (raw, allocation)
    };
    let uncovered: Vec<String> = allocation
        .iter()
        .filter(|n| !ALLOCATORS.contains(&n.as_str()))
        .cloned()
        .collect();
    Ok(Value::object([
        ("raw_undefined_symbols", strs(raw)),
        ("allocator_imports", strs(allocation)),
        ("uncovered", strs(uncovered)),
        ("raw_listing", Value::str(listing)),
    ]))
}

// ---------------------------------------------------------------------------
// Loader environment and the resolved ELF closure.

/// Loader-relevant variables; inherited interposers are refused outright.
pub fn loader_environment() -> R<Value> {
    if ["LD_PRELOAD", "LD_AUDIT"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
    {
        return Err("inherited LD_PRELOAD/LD_AUDIT changes instrumentation".into());
    }
    if let Ok(text) = fs::read_to_string("/etc/ld.so.preload")
        && text
            .lines()
            .any(|l| !l.split('#').next().unwrap_or_default().trim().is_empty())
    {
        return Err("system-wide ld.so.preload is not covered".into());
    }
    let mut vars: Vec<(String, Value)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("LD_") || k.starts_with("MALLOC_") || k == "GLIBC_TUNABLES")
        .map(|(k, v)| (k, Value::str(v)))
        .collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Value::Object(vars))
}

fn run(program: &str, args: &[&str], path: &Path) -> R<String> {
    let output = Command::new(program)
        .args(args)
        .arg(path)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            path.display(),
            output.status
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

fn bracketed<'a>(text: &'a str, marker: &str) -> Vec<&'a str> {
    text.lines()
        .filter_map(|l| {
            let rest = &l[l.find(marker)? + marker.len()..];
            // The pattern's greedy prefix selects the last bracketed field.
            let start = rest.rfind('[')? + 1;
            let end = start + rest[start..].find(']')?;
            (end > start).then(|| &rest[start..end])
        })
        .collect()
}

fn interpreters(headers: &str) -> Vec<&str> {
    let marker = "[Requesting program interpreter: ";
    headers
        .lines()
        .filter_map(|l| {
            l.split_once(marker)
                .and_then(|(_, r)| r.split_once(']'))
                .map(|(p, _)| p)
        })
        .collect()
}

fn canonical(path: &str) -> String {
    fs::canonicalize(path).map_or_else(|_| path.to_owned(), |p| p.display().to_string())
}

/// `.so` followed by any number of `.<digits>` groups.
fn so_suffix(text: &str) -> bool {
    text.strip_prefix(".so").is_some_and(|versions| {
        versions.is_empty()
            || (versions.starts_with('.')
                && versions[1..]
                    .split('.')
                    .all(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())))
    })
}

/// An ELF interpreter file name: `ld.so`, `ld.so.2`, `ld-linux-aarch64.so.1`, ...
fn loader_like(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("ld") else {
        return false;
    };
    so_suffix(rest)
        || rest.strip_prefix("-linux").is_some_and(|tail| {
            (0..=tail.len())
                .filter(|i| tail.is_char_boundary(*i))
                .any(|i| !tail[..i].contains('/') && so_suffix(&tail[i..]))
        })
}

/// The ELF class of objects built for this host: 64-bit on AArch64 and x86_64,
/// 32-bit on ARMv7.
const NATIVE_CLASS: &str = if cfg!(target_pointer_width = "64") {
    "ELF64"
} else {
    "ELF32"
};

/// Every DT_NEEDED file of the inspected native-class DYN object resolves inside the
/// listed closure, and nothing unexplained is listed.
fn main_elf_closure(worker: &Path, libraries: &[Value], program_headers: &str) -> R<Value> {
    if libraries.len() > 128 {
        return Err("ELF dependency closure exceeds 128 files".into());
    }
    let first_load = program_headers.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        (f.next() == Some("LOAD"))
            .then(|| (f.next().map(str::to_owned), f.next().map(str::to_owned)))
    });
    let zero = |v: &Option<String>| {
        v.as_deref()
            .and_then(|t| t.strip_prefix("0x"))
            .is_some_and(|h| u64::from_str_radix(h, 16) == Ok(0))
    };
    if !first_load.as_ref().is_some_and(|(a, b)| zero(a) && zero(b)) {
        return Err("anonymous main mapping requires first ELF LOAD offset/vaddr zero".into());
    }
    let root = canonical(&worker.display().to_string());
    let raw = run("readelf", &["--file-header", "--dynamic", "--wide"], worker)?;
    let header = |key: &str, value: &str| {
        raw.lines().any(|l| {
            l.trim()
                .strip_prefix(key)
                .is_some_and(|r| r.trim().starts_with(value))
        })
    };
    if !header("Class:", NATIVE_CLASS) || !header("Type:", "DYN") {
        return Err(format!(
            "anonymous main mapping requires an inspected {NATIVE_CLASS} DYN object"
        ));
    }
    let paths: BTreeSet<String> = libraries
        .iter()
        .filter_map(|r| s(r, "realpath").map(str::to_owned))
        .collect();
    let mut dynamic = BTreeMap::new();
    dynamic.insert(root.clone(), raw);
    for path in paths.iter().filter(|p| **p != root) {
        dynamic.insert(
            path.clone(),
            run("readelf", &["--dynamic", "--wide"], Path::new(path))?,
        );
    }
    let mut facts = BTreeMap::new();
    for (path, text) in &dynamic {
        if !text.contains("Dynamic section") {
            return Err(format!("unreadable ELF dynamic section: {path}"));
        }
        let needed = bracketed(text, "(NEEDED)");
        let soname = bracketed(text, "(SONAME)");
        if needed.len() > 128
            || soname.len() > 1
            || text.matches("(NEEDED)").count() != needed.len()
        {
            return Err(format!(
                "unparsed or excessive ELF DT_NEEDED records: {path}"
            ));
        }
        facts.insert(
            path.clone(),
            (
                needed.iter().map(ToString::to_string).collect::<Vec<_>>(),
                soname.first().map(ToString::to_string),
            ),
        );
    }
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    for row in libraries {
        let path = s(row, "realpath").unwrap_or_default().to_owned();
        let listed = s(row, "path").unwrap_or_default();
        let base = Path::new(listed)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let soname = facts.get(&path).and_then(|f| f.1.clone());
        for name in [
            s(row, "name").map(str::to_owned),
            Some(listed.to_owned()),
            Some(path.clone()),
            base,
            soname,
        ]
        .into_iter()
        .flatten()
        {
            if aliases.get(&name).is_some_and(|p| *p != path) {
                return Err(format!("ambiguous ELF dependency name: {name}"));
            }
            aliases.insert(name, path.clone());
        }
    }
    let (mut visited, mut pending) = (BTreeSet::new(), vec![root.clone()]);
    while let Some(path) = pending.pop() {
        if !visited.insert(path.clone()) {
            continue;
        }
        for name in &facts.get(&path).ok_or("ELF facts missing")?.0 {
            pending.push(
                aliases
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("unresolved ELF DT_NEEDED: {name}"))?,
            );
        }
    }
    let mut implicit: BTreeSet<String> = interpreters(program_headers)
        .into_iter()
        .map(canonical)
        .collect();
    implicit.extend(
        libraries
            .iter()
            .filter(|r| {
                s(r, "path")
                    .and_then(|p| Path::new(p).file_name())
                    .is_some_and(|n| loader_like(&n.to_string_lossy()))
            })
            .filter_map(|r| s(r, "realpath").map(str::to_owned)),
    );
    let extra: Vec<&String> = paths
        .iter()
        .filter(|p| !visited.contains(*p) && !implicit.contains(*p))
        .collect();
    if !extra.is_empty() {
        return Err(format!("unexplained extra ELF dependency files: {extra:?}"));
    }
    let closure = Value::object(facts.into_iter().map(|(path, (needed, soname))| {
        (
            path,
            Value::object([
                ("needed", strs(needed)),
                ("soname", soname.map_or(Value::Null, Value::str)),
            ]),
        )
    }));
    Ok(Value::object([
        ("inspected_class", Value::str(NATIVE_CLASS)),
        ("inspected_type", Value::str("DYN")),
        ("first_LOAD_vaddr", Value::int(0)),
        ("needed_closure", closure),
        (
            "raw_dynamic_sections",
            Value::object(dynamic.into_iter().map(|(k, v)| (k, Value::str(v)))),
        ),
    ]))
}

fn host(flag: &str) -> String {
    Command::new("uname")
        .arg(flag)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

pub fn kernel_release() -> String {
    host("-r")
}

pub fn machine() -> String {
    host("-m")
}

/// Resolve `worker`'s dependencies with its ELF interpreter (`--list` never runs
/// the worker's main) and hash every file of the closure.
pub fn linked_libraries(worker: &Path, interpreter: Option<&str>) -> R<Value> {
    use std::os::unix::fs::PermissionsExt;
    let root_sha = file_sha(worker)?;
    let binding = loader_environment()?;
    if fs::metadata(worker)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode()
        & 0o6000
        != 0
    {
        return Err("secure-execution worker cannot certify LD_PRELOAD".into());
    }
    let headers = run("readelf", &["--program-headers", "--wide"], worker)?;
    let found = interpreters(&headers);
    if interpreter.is_none() && found.len() != 1 {
        return Err("missing or ambiguous ELF interpreter; preload coverage unproven".into());
    }
    if let Some(wanted) = interpreter
        && (found.len() > 1 || found.first().is_some_and(|f| *f != wanted))
    {
        return Err("ELF interpreter differs from calibration executable".into());
    }
    // A shared object such as the observer has no PT_INTERP of its own; it is
    // resolved with the interpreter of the executable it is preloaded into.
    let loader = PathBuf::from(interpreter.unwrap_or_else(|| found[0]));
    if !loader.is_absolute() || !loader.is_file() {
        return Err("unavailable ELF interpreter".into());
    }
    let listing = run(&loader.display().to_string(), &["--list"], worker)?;
    let lines: Vec<&str> = listing.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() > 256 {
        return Err("ELF listing exceeds 256 records".into());
    }
    let (mut libraries, mut virtual_libraries, mut anonymous) =
        (Vec::new(), Vec::new(), None::<Value>);
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if line.starts_with(char::is_whitespace)
            && let Some(address) = trimmed
                .strip_prefix("(0x")
                .and_then(|r| r.strip_suffix(')'))
            && !address.is_empty()
            && address.len() <= 16
            && address.bytes().all(|b| b.is_ascii_hexdigit())
        {
            if index != 0 || anonymous.is_some() || u64::from_str_radix(address, 16) == Ok(0) {
                return Err("unexpected or duplicate anonymous ELF mapping".into());
            }
            anonymous = Some(Value::object([
                ("line_raw", Value::str(*line)),
                ("address_raw", Value::str(format!("0x{address}"))),
            ]));
            continue;
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        let address_ok = |a: &str| {
            a.strip_prefix("(0x")
                .and_then(|r| r.strip_suffix(')'))
                .is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
        };
        let (name, value) = match fields.as_slice() {
            [name, "=>", value, address] if address_ok(address) => (Some(*name), *value),
            [value, address] if address_ok(address) => (None, *value),
            _ => return Err(format!("unparsed or unresolved ELF dependency: {line}")),
        };
        if name.is_none() && matches!(value, "linux-vdso.so.1" | "linux-gate.so.1") {
            virtual_libraries.push(value.to_owned());
            continue;
        }
        let path = Path::new(value);
        if !path.is_absolute() || !path.is_file() {
            return Err(format!("unreadable ELF dependency: {value}"));
        }
        let file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        libraries.push(Value::object([
            ("name", Value::str(name.map_or(file, str::to_owned))),
            ("path", Value::str(value)),
            ("realpath", Value::str(canonical(value))),
            (
                "bytes",
                Value::int(fs::metadata(path).map_err(|e| e.to_string())?.len() as i64),
            ),
            ("sha256", Value::str(file_sha(path)?)),
        ]));
    }
    if libraries.is_empty() {
        return Err("no resolved ELF dependency files".into());
    }
    let loader_real = canonical(&loader.display().to_string());
    if !libraries
        .iter()
        .any(|r| s(r, "realpath") == Some(loader_real.as_str()))
    {
        libraries.push(Value::object([
            (
                "name",
                Value::str(loader.file_name().unwrap_or_default().to_string_lossy()),
            ),
            ("path", Value::str(loader.display().to_string())),
            ("realpath", Value::str(&loader_real)),
            (
                "bytes",
                Value::int(fs::metadata(&loader).map_err(|e| e.to_string())?.len() as i64),
            ),
            ("sha256", Value::str(file_sha(&loader)?)),
        ]));
    }
    let main = main_elf_closure(worker, &libraries, &headers)?;
    if file_sha(worker)? != root_sha {
        return Err("inspected ELF changed during dependency discovery".into());
    }
    let anonymous = anonymous.map_or(Value::Null, |a| {
        let mut fields = match a {
            Value::Object(m) => m,
            _ => vec![],
        };
        if let Value::Object(m) = &main {
            fields.extend(m.iter().cloned());
        }
        Value::Object(fields)
    });
    Ok(Value::object([
        ("status", Value::str("recorded")),
        ("dynamic_links", Value::str(&listing)),
        ("program_headers", Value::str(&headers)),
        (
            "inspected_artifact",
            Value::object([
                ("path", Value::str(canonical(&worker.display().to_string()))),
                ("sha256", Value::str(root_sha)),
            ]),
        ),
        ("main_elf_inspection", main),
        ("anonymous_main_mapping", anonymous),
        ("interpreter", Value::str(loader.display().to_string())),
        ("interpreter_sha256", Value::str(file_sha(&loader)?)),
        ("libraries", Value::Array(libraries)),
        ("virtual_kernel_libraries", strs(virtual_libraries)),
        ("kernel_release", Value::str(kernel_release())),
        ("binding_environment", binding),
    ]))
}

/// The content identity of a closure (files and environment), without loader
/// addresses.
pub fn library_identity(data: &Value) -> Value {
    let keys = [
        "status",
        "interpreter",
        "interpreter_sha256",
        "libraries",
        "inspected_artifact",
        "virtual_kernel_libraries",
        "kernel_release",
        "binding_environment",
    ];
    let mut fields: Vec<(String, Value)> = keys
        .iter()
        .filter_map(|k| data.get(k).map(|v| (k.to_string(), v.clone())))
        .collect();
    let inspection = data
        .get("main_elf_inspection")
        .filter(|v| **v != Value::Null)
        .or_else(|| {
            data.get("anonymous_main_mapping")
                .filter(|v| **v != Value::Null)
        });
    if let Some(inspection) = inspection {
        let main = [
            "inspected_class",
            "inspected_type",
            "first_LOAD_vaddr",
            "needed_closure",
        ]
        .map(|k| (k, inspection.get(k).cloned().unwrap_or(Value::Null)));
        fields.push(("inspected_main_ELF".into(), Value::object(main)));
    }
    Value::Object(fields)
}

pub fn linked_hashes(linked: &Value) -> Value {
    let Value::Object(roles) = linked else {
        return Value::Object(vec![]);
    };
    Value::Object(
        roles
            .iter()
            .map(|(name, data)| {
                let rows = data
                    .get("libraries")
                    .and_then(Value::as_array)
                    .unwrap_or_default();
                let map = rows.iter().map(|r| {
                    (
                        s(r, "realpath").unwrap_or_default().to_owned(),
                        r.get("sha256").cloned().unwrap_or(Value::Null),
                    )
                });
                (name.clone(), Value::Object(map.collect()))
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Observer receipts.

pub fn resident_basis_valid(data: &Value, mode: &str) -> bool {
    let Some(requested) = py_int(data.get("kernel_rss_requested")).or(
        if data.get("kernel_rss_requested").is_none() {
            Some(0)
        } else {
            None
        },
    ) else {
        return false;
    };
    if requested != 0 && requested != 1 {
        return false;
    }
    let basis = s(data, "rss_peak_upper_bound_basis");
    if requested == 0 {
        return basis != Some(KERNEL_RSS_BASIS)
            && (mode == "heap"
                || (basis == Some("process-lifetime-VmPeak")
                    && match (
                        data.get("rss_peak_upper_bound_bytes"),
                        data.get("virtual_peak_end"),
                    ) {
                        (Some(a), Some(b)) => equal(a, b),
                        (None, None) => true,
                        _ => false,
                    }));
    }
    if mode != "rss"
        || s(data, "observer") != Some("linux-glibc-v3")
        || basis != Some(KERNEL_RSS_BASIS)
    {
        return false;
    }
    let keys = [
        "kernel_rss_abi",
        "kernel_rss_initial_bytes",
        "kernel_rss_positive_pages",
        "kernel_rss_upper_bytes",
        "kernel_rss_events",
        "kernel_rss_missed",
        "kernel_rss_errors",
        "kernel_rss_page_size",
    ];
    let Some(v): Option<Vec<i128>> = keys
        .iter()
        .map(|k| py_int(data.get(k)).filter(|n| (0..1i128 << 64).contains(n)))
        .collect()
    else {
        return false;
    };
    let [
        abi,
        initial,
        pages,
        upper_bytes,
        events,
        missed,
        errors,
        page,
    ] = v[..]
    else {
        return false;
    };
    if page <= 0 {
        return false;
    }
    let upper = initial + pages * page;
    // Thread creation is closed for the whole observer lifetime, not only the window.
    let guard_ok = data
        .get("guard_calls")
        .and_then(Value::as_array)
        .is_some_and(|g| g.len() == 7 && py_int(g.get(3)) == Some(0));
    let virtual_end = py_int(data.get("virtual_peak_end")).unwrap_or(-1);
    abi == 1
        && missed == 0
        && errors == 0
        && Some(page) == py_int(data.get("page_size"))
        && initial % page == 0
        && upper < 1 << 64
        && upper == upper_bytes
        && (events > 0 || pages == 0)
        && guard_ok
        && py_int(data.get("rss_peak_upper_bound_bytes")) == Some(upper.min(virtual_end))
}

pub fn valid_receipt(data: &Value, mode: &str) -> bool {
    let check = || -> Option<bool> {
        if !matches!(data, Value::Object(_)) {
            return Some(false);
        }
        let observer = s(data, "observer")?;
        let fixed = [
            ("rss_source", "proc-self-smaps_rollup-Rss"),
            (
                "hwm_source",
                "getrusage-RUSAGE_SELF-ru_maxrss-KiB-times-1024",
            ),
            ("hwm_scope", "process-lifetime-reported"),
            ("rss_exact_peak_status", "unresolved"),
            ("hwm_upper_bound_status", "unproven"),
            ("virtual_peak_source", "proc-self-status-VmPeak"),
            ("rss_bound_scope", "same-process-ordinary-mappings"),
            (
                "rss_bound_formula",
                "candidate-ceiling-minus-start-plus-two-pages-v1",
            ),
        ];
        let numeric = [
            "heap_peak_bytes",
            "heap_live_start",
            "heap_live_end",
            "calls",
            "window_allocation_calls",
            "rss_start",
            "rss_peak_observed",
            "rss_end",
            "hwm_start",
            "hwm_end",
            "page_size",
            "virtual_peak_start",
            "virtual_peak_end",
            "rss_samples",
            "error_bits",
            "begins",
            "ends",
            "unknown_releases",
            "recursive_calls",
            "selected_thread_scope",
            "entry_capacity",
        ];
        let n = |k: &str| py_int(data.get(k));
        let ok = py_int(data.get("schema")) == Some(2)
            && s(data, "mode") == Some(mode)
            && s(data, "status") == Some("measured")
            && matches!(observer, "linux-glibc-v2" | "linux-glibc-v3")
            && (observer != "linux-glibc-v3" || guard_component_valid(data))
            && matches!(mode, "heap" | "rss")
            && fixed.iter().all(|(k, v)| s(data, k) == Some(*v))
            && n("error_bits") == Some(0)
            && n("begins") == Some(1)
            && n("ends") == Some(1)
            && n("recursive_calls") == Some(0)
            && n("selected_thread_scope") == Some(1)
            && data.get("glibc").is_some_and(Value::truthy)
            && [6, ALLOCATORS.len()].into_iter().any(|hooks| {
                data.get("symbol_order")
                    .is_some_and(|o| equal(o, &Value::strings(ALLOCATORS[..hooks].iter().copied())))
                    && ["symbol_attempts", "symbol_successes", "symbol_failures"]
                        .iter()
                        .all(|k| {
                            ints(data.get(k), hooks).is_some_and(|v| v.iter().all(|x| *x >= 0))
                        })
            })
            && numeric.iter().all(|k| n(k).is_some_and(|v| v >= 0));
        if !ok {
            return Some(false);
        }
        let page = n("page_size")?;
        Some(
            page > 0
                && page & (page - 1) == 0
                && n("hwm_end")? >= n("hwm_start")?
                && n("virtual_peak_end")? >= n("virtual_peak_start")?
                && n("rss_peak_observed")? >= n("rss_start")?.max(n("rss_end")?)
                && resident_basis_valid(data, mode)
                && (mode == "heap"
                    || n("rss_peak_upper_bound_bytes")
                        .is_some_and(|u| u >= n("rss_peak_observed").unwrap_or(i128::MAX))),
        )
    };
    check().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The runtime guard certificate.

/// Sealed files of all closures, their path spellings, and the one libc and
/// ELF interpreter they share.
pub fn closure_facts(linked: &Value) -> R<Value> {
    let Value::Object(roles) = linked else {
        return Err("unrecorded library closure".into());
    };
    let (mut files, mut aliases): (Members, Members) = (Vec::new(), Vec::new());
    let (mut libcs, mut loaders) = (BTreeSet::new(), BTreeSet::new());
    let set = |list: &mut Vec<(String, Value)>, key: &str, value: Value, conflict: &str| -> R<()> {
        match list.iter().find(|(k, _)| k == key) {
            Some((_, old)) if *old != value => Err(conflict.into()),
            Some(_) => Ok(()),
            None => {
                list.push((key.to_owned(), value));
                Ok(())
            }
        }
    };
    for (_, data) in roles {
        if !matches!(data, Value::Object(_)) || s(data, "status") != Some("recorded") {
            return Err("unrecorded library closure".into());
        }
        for row in data
            .get("libraries")
            .and_then(Value::as_array)
            .ok_or("'libraries'")?
        {
            let path = s(row, "realpath").ok_or("'realpath'")?;
            let content = row.get("sha256").and_then(Value::as_str);
            let sealed = content.is_some_and(|c| {
                c.len() == 64
                    && c.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
            if !Path::new(path).is_absolute() || !sealed {
                return Err("unsealed library file".into());
            }
            let content = content.unwrap();
            set(
                &mut files,
                path,
                Value::str(content),
                "conflicting library content",
            )?;
            for spelling in [s(row, "path").ok_or("'path'")?, path] {
                set(
                    &mut aliases,
                    spelling,
                    Value::str(path),
                    "ambiguous library path",
                )?;
            }
            if s(row, "name") == Some("libc.so.6") {
                libcs.insert(path.to_owned());
            }
            let interpreter = s(data, "interpreter");
            if s(data, "interpreter_sha256") == Some(content)
                && (interpreter == s(row, "path") || interpreter == Some(path))
            {
                loaders.insert(path.to_owned());
            }
        }
        if let Some(artifact) = data.get("inspected_artifact").filter(|a| a.truthy()) {
            let path = s(artifact, "path").ok_or("'path'")?;
            set(
                &mut files,
                path,
                artifact.get("sha256").cloned().ok_or("'sha256'")?,
                "conflicting inspected artifact",
            )?;
        }
    }
    if libcs.len() != 1 || loaders.len() != 1 {
        return Err("one sealed libc/ELF interpreter pair required".into());
    }
    Ok(Value::object([
        ("files", Value::Object(files)),
        ("aliases", Value::Object(aliases)),
        ("libc", Value::str(libcs.into_iter().next().unwrap())),
        ("loader", Value::str(loaders.into_iter().next().unwrap())),
    ]))
}

/// Symbol name to its set of versions (`name@@VER`, `name@VER` or unversioned).
pub fn symbol_versions(listing: &str) -> R<BTreeMap<String, BTreeSet<String>>> {
    let mut result: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in listing.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            return Err("unparsed GNU symbol record".into());
        }
        let (name, version) = fields[0]
            .split_once('@')
            .map_or((fields[0], ""), |(n, v)| (n, v));
        result
            .entry(name.to_owned())
            .or_default()
            .insert(version.trim_start_matches('@').to_owned());
    }
    Ok(result)
}

fn binding_facts(bindings: &Value, facts: &Value) -> R<Value> {
    let defined_raw = match bindings.get("defined_objects") {
        Some(Value::Object(m)) => m.clone(),
        _ => return Err("'defined_objects'".into()),
    };
    let file_keys: BTreeSet<&str> = match facts.get("files") {
        Some(Value::Object(m)) => m.iter().map(|(k, _)| k.as_str()).collect(),
        _ => return Err("'files'".into()),
    };
    if defined_raw
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<BTreeSet<_>>()
        != file_keys
    {
        return Err("incomplete/extra export-audited object closure".into());
    }
    let mut defined = Vec::new();
    for (path, raw) in &defined_raw {
        defined.push((
            path.clone(),
            symbol_versions(raw.as_str().ok_or("export transcript is not text")?)?,
        ));
    }
    let exports = s(bindings, "observer_exports").ok_or("'observer_exports'")?;
    let observer_path = s(bindings, "observer_path").ok_or("'observer_path'")?;
    let observer = symbol_versions(exports)?;
    if defined_raw
        .iter()
        .find(|(k, _)| k == observer_path)
        .and_then(|(_, v)| v.as_str())
        != Some(exports)
    {
        return Err("observer export transcript mismatch".into());
    }
    let tested = symbol_versions(s(bindings, "probe_undefined").ok_or("'probe_undefined'")?)?;
    let (libc, loader) = (
        s(facts, "libc").unwrap_or_default(),
        s(facts, "loader").unwrap_or_default(),
    );
    let versions_of = |path: &str, name: &str| {
        defined
            .iter()
            .find(|(p, _)| p == path)
            .and_then(|(_, m)| m.get(name))
            .cloned()
            .unwrap_or_default()
    };
    for name in GUARD_ORDER {
        if !observer.contains_key(name) || tested.get(name).is_none_or(BTreeSet::is_empty) {
            return Err(format!("missing exported or exercised guard: {name}"));
        }
        if defined.iter().any(|(path, m)| {
            m.contains_key(name) && ![libc, loader, observer_path].contains(&path.as_str())
        }) {
            return Err("unexpected public guard provider".into());
        }
        let foundation: BTreeSet<String> = versions_of(libc, name)
            .union(&versions_of(loader, name))
            .cloned()
            .collect();
        if !tested[name].is_subset(&foundation) {
            return Err("tested guard ABI version has no sealed foundation provider".into());
        }
    }
    for name in TLS_IMPORTS {
        let providers: Vec<&str> = defined
            .iter()
            .filter(|(_, m)| m.contains_key(name))
            .map(|(p, _)| p.as_str())
            .collect();
        if providers != [loader] || !versions_of(loader, name).contains("GLIBC_PRIVATE") {
            return Err("private TLS provider is not uniquely the sealed interpreter".into());
        }
    }
    Ok(Value::object([
        (
            "tested_guard_versions",
            Value::object(GUARD_ORDER.map(|n| (n, strs(tested[n].iter().cloned())))),
        ),
        ("binding_evidence_sha256", Value::str(digest(bindings))),
    ]))
}

fn guard_fields(data: &Value) -> R<()> {
    let n = |k: &str| py_int(data.get(k));
    let profile = matches!(data, Value::Object(_))
        && n("schema") == Some(2)
        && s(data, "observer") == Some("linux-glibc-v3")
        && s(data, "guard_policy") == Some(GUARD_POLICY)
        && s(data, "guard_scope") == Some("observed-interposable-entrypoints")
        && data
            .get("guard_order")
            .is_some_and(|o| equal(o, &Value::strings(GUARD_ORDER)))
        && n("guard_resolved_mask") == Some(127)
        && s(data, "private_tls_basis") == Some("no-active-thread-or-loader-entrypoints")
        && is_false(data.get("private_tls_direct_hooks"))
        && n("begins") == Some(1)
        && n("ends") == Some(1)
        && n("selected_thread_scope") == Some(1);
    if !profile {
        return Err("missing/stale guard profile, scope or boundary".into());
    }
    let mut counters = Vec::new();
    for key in [
        "guard_calls",
        "guard_window_calls",
        "guard_window_successes",
        "guard_window_failures",
    ] {
        let values =
            ints(data.get(key), 7).filter(|v| v.iter().all(|x| (0..1i128 << 64).contains(x)));
        counters.push(values.ok_or_else(|| format!("malformed guard counters: {key}"))?);
    }
    for i in 0..7 {
        let (total, active, ok, failed) = (
            counters[0][i],
            counters[1][i],
            counters[2][i],
            counters[3][i],
        );
        if total < active || ok + failed != active {
            return Err("inconsistent guard counters".into());
        }
    }
    Ok(())
}

fn zero_active(data: &Value) -> R<()> {
    guard_fields(data)?;
    let active = ints(data.get("guard_window_calls"), 7).unwrap_or_default();
    if s(data, "status") != Some("measured")
        || py_int(data.get("error_bits")) != Some(0)
        || py_int(data.get("unknown_window_releases")) != Some(0)
        || active.iter().any(|v| *v != 0)
    {
        return Err("active or invalid guard window".into());
    }
    Ok(())
}

pub fn guard_component_valid(data: &Value) -> bool {
    zero_active(data).is_ok()
}

fn expected_controls() -> Vec<(String, String)> {
    let mut keys = Vec::new();
    for mode in ["heap", "rss"] {
        for case in ["default", "scope-startup"]
            .into_iter()
            .chain(GUARD_CASES.iter().map(|c| c.0))
        {
            keys.push((mode.to_owned(), case.to_owned()));
        }
    }
    keys
}

/// Certify the active-window guard from its 28 native calibration records.
pub fn certify(
    records: &[Value],
    linked: &Value,
    observer_sha: &str,
    probe_sha: &str,
    bindings: &Value,
) -> R<Value> {
    let facts = closure_facts(linked)?;
    let file = |k: &str| {
        facts
            .get("files")
            .and_then(|f| f.get(k))
            .and_then(Value::as_str)
    };
    if file(s(bindings, "observer_path").unwrap_or_default()) != Some(observer_sha)
        || file(s(bindings, "probe_path").unwrap_or_default()) != Some(probe_sha)
    {
        return Err("guard export/probe transcript is not bound to the calibrated files".into());
    }
    let bound = binding_facts(bindings, &facts)?;
    let expected = expected_controls();
    let mut seen: Vec<(String, String)> = Vec::new();
    for record in records {
        let key = (
            s(record, "mode").ok_or("'mode'")?.to_owned(),
            s(record, "case").ok_or("'case'")?.to_owned(),
        );
        if !expected.contains(&key) || seen.contains(&key) {
            return Err("duplicate/unexpected guard calibration".into());
        }
        let observed = record.get("observed").ok_or("'observed'")?;
        if s(observed, "instrument_sha256") != Some(observer_sha) {
            return Err("calibration observer changed".into());
        }
        let data = observed
            .get("observer")
            .or_else(|| observed.get("receipt"))
            .unwrap_or(&Value::Null);
        guard_fields(data)?;
        if s(data, "mode") != Some(key.0.as_str()) {
            return Err("calibration mode mismatch".into());
        }
        let empty = Value::Object(vec![]);
        let work = observed.get("work").unwrap_or(&empty);
        let case = key.1.as_str();
        if s(work, "calibration") != Some(case) {
            return Err("wrong native probe control".into());
        }
        let n = |v: &Value, k: &str| py_int(v.get(k));
        if case == "default" || case == "scope-startup" {
            if s(observed, "status") != Some("measured")
                || !is_false(work.get("expect_guard_invalid"))
            {
                return Err("positive guard calibration failed".into());
            }
            zero_active(data)?;
            let same_live = match (data.get("heap_live_end"), data.get("heap_live_start")) {
                (Some(a), Some(b)) => equal(a, b),
                (None, None) => true,
                _ => false,
            };
            let failed = !is_true(work.get("expected_release_equal"))
                || !same_live
                || (key.0 == "heap"
                    && (n(work, "expected_peak_bytes").is_none()
                        || !data
                            .get("heap_peak_bytes")
                            .is_some_and(|p| equal(p, work.get("expected_peak_bytes").unwrap()))))
                || (key.0 == "rss"
                    && !matches!((n(data, "rss_start"), n(data, "rss_peak_observed")), (Some(a), Some(b)) if b > a));
            if failed {
                return Err("positive allocation/release/resident oracle failed".into());
            }
            if case == "scope-startup"
                && !ints(data.get("guard_calls"), 7).is_some_and(|g| g.iter().all(|v| *v > 0))
            {
                return Err("startup forwarding did not exercise every guard".into());
            }
        } else {
            let (_, index, successful) = *GUARD_CASES
                .iter()
                .find(|c| c.0 == case)
                .expect("guard case");
            let mut requested = vec![0i64; 7];
            requested[index] = 1;
            let (succeeded, failed) = if successful {
                (requested.clone(), vec![0; 7])
            } else {
                (vec![0; 7], requested.clone())
            };
            let list = |v: &[i64]| Value::Array(v.iter().map(|x| Value::int(*x)).collect());
            let oracle = is_true(work.get("expect_guard_invalid"))
                && s(work, "guard_oracle_scope") == Some("authored-top-level-request-minima")
                && work
                    .get("required_guard_window_calls")
                    .is_some_and(|v| equal(v, &list(&requested)))
                && work
                    .get("required_guard_window_successes")
                    .is_some_and(|v| equal(v, &list(&succeeded)))
                && work
                    .get("required_guard_window_failures")
                    .is_some_and(|v| equal(v, &list(&failed)));
            if !oracle {
                return Err("probe oracle differs from the pinned top-level control".into());
            }
            let below = |key: &str, minimum: &[i64]| {
                ints(data.get(key), 7).is_none_or(|actual| {
                    actual.iter().zip(minimum).any(|(a, m)| *a < i128::from(*m))
                })
            };
            let sticky = s(observed, "status") == Some("observer-invalid")
                && s(data, "status") == Some("invalid")
                && n(data, "error_bits").is_some_and(|b| b & SCOPE_ERROR_BIT != 0)
                && !below("guard_window_calls", &requested)
                && !below("guard_window_successes", &succeeded)
                && !below("guard_window_failures", &failed);
            if !sticky {
                return Err("negative control did not prove sticky guard rejection".into());
            }
        }
        seen.push(key);
    }
    if seen.len() != expected.len() {
        return Err("missing guard calibration controls".into());
    }
    let mut certificate = vec![
        ("schema", Value::str("linux-runtime-guard-certificate-v3")),
        ("status", Value::str("qualified-active-guards")),
        ("observer_sha256", Value::str(observer_sha)),
        ("probe_sha256", Value::str(probe_sha)),
        ("guard_policy", Value::str(GUARD_POLICY)),
        ("closure", facts.clone()),
        ("closure_sha256", Value::str(digest(&facts))),
        ("binding_facts", bound),
        ("binding_evidence", bindings.clone()),
        ("native_calibration_records", Value::Array(records.to_vec())),
        (
            "TLS_proof",
            Value::str(
                "sealed libc/interpreter plus closed active thread/loader paths; private symbols are not interposed",
            ),
        ),
    ];
    let sha = digest(&Value::object(certificate.clone()));
    certificate.push(("certificate_sha256", Value::str(sha)));
    Ok(Value::object(certificate))
}

pub fn certificate_valid(
    certificate: Option<&Value>,
    linked: &Value,
    observer_sha: Option<&str>,
    probe_sha: Option<&str>,
) -> bool {
    let Some(certificate) = certificate.filter(|c| matches!(c, Value::Object(_))) else {
        return false;
    };
    let (Some(observer), Some(probe)) = (observer_sha, probe_sha) else {
        return false;
    };
    if s(certificate, "status") != Some("qualified-active-guards") {
        return false;
    }
    let (Some(Value::Array(records)), Some(bindings)) = (
        certificate.get("native_calibration_records"),
        certificate.get("binding_evidence"),
    ) else {
        return false;
    };
    certify(records, linked, observer, probe, bindings)
        .is_ok_and(|again| equal(&again, certificate))
}

/// Coverage of otherwise uncovered imports, from the certificate's sealed
/// evidence: the guard (public guarded entry points at tested versions), the
/// sealed libc's private TLS imports, an operator new or delete form defined
/// by exactly one sealed object and calibrated by the probe at the imported
/// versions (the probe's exact peak shows it reaches the hooks), and a weak
/// import no sealed object defines, which resolves to null.
pub fn conditional_coverage(
    parsed: &Value,
    owner: &Value,
    certificate: Option<&Value>,
    linked: &Value,
    observer_sha: Option<&str>,
    probe_sha: Option<&str>,
) -> Value {
    let uncovered: BTreeSet<String> = parsed
        .get("uncovered")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let mut covered: BTreeMap<String, Value> = BTreeMap::new();
    if certificate_valid(certificate, linked, observer_sha, probe_sha) {
        let certificate = certificate.unwrap();
        let sha = certificate
            .get("certificate_sha256")
            .cloned()
            .unwrap_or(Value::Null);
        let versions =
            symbol_versions(s(parsed, "raw_listing").unwrap_or_default()).unwrap_or_default();
        let tested = certificate
            .get("binding_facts")
            .and_then(|b| b.get("tested_guard_versions"));
        for name in GUARD_ORDER.iter().filter(|n| uncovered.contains(**n)) {
            let allowed: BTreeSet<String> = tested
                .and_then(|t| t.get(name))
                .and_then(Value::as_array)
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if versions
                .get(*name)
                .is_some_and(|v| !v.is_empty() && v.is_subset(&allowed))
            {
                covered.insert(
                    name.to_string(),
                    Value::object([
                        ("method", Value::str("tested-public-entry-window-guard")),
                        ("certificate", sha.clone()),
                    ]),
                );
            }
        }
        let facts = certificate.get("closure").unwrap_or(&Value::Null);
        let libc = s(facts, "libc");
        if s(owner, "realpath") == libc
            && owner.get("sha256") == libc.and_then(|l| facts.get("files").and_then(|f| f.get(l)))
        {
            let raw: BTreeSet<&str> = parsed
                .get("raw_undefined_symbols")
                .and_then(Value::as_array)
                .unwrap_or_default()
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for name in TLS_IMPORTS.iter().filter(|n| uncovered.contains(**n)) {
                let private: BTreeSet<String> = ["GLIBC_PRIVATE".to_owned()].into();
                if raw.contains(format!("{name}@GLIBC_PRIVATE").as_str())
                    && versions.get(*name) == Some(&private)
                {
                    covered.insert(
                        name.to_string(),
                        Value::object([
                            ("method", Value::str("sealed-libc-loader-startup-scope")),
                            ("certificate", sha.clone()),
                        ]),
                    );
                }
            }
        }
        let evidence = certificate.get("binding_evidence").unwrap_or(&Value::Null);
        let exports: Vec<(&str, BTreeMap<String, BTreeSet<String>>)> =
            match evidence.get("defined_objects") {
                Some(Value::Object(objects)) => objects
                    .iter()
                    .map(|(path, raw)| {
                        let listing = raw.as_str().unwrap_or_default();
                        (path.as_str(), symbol_versions(listing).unwrap_or_default())
                    })
                    .collect(),
                _ => Vec::new(),
            };
        let providers = |name: &str| -> Vec<(&str, &BTreeSet<String>)> {
            exports
                .iter()
                .filter_map(|(path, symbols)| symbols.get(name).map(|v| (*path, v)))
                .collect()
        };
        let calibrated =
            symbol_versions(s(evidence, "probe_undefined").unwrap_or_default()).unwrap_or_default();
        let observer = s(evidence, "observer_path").unwrap_or_default();
        for name in uncovered.iter().filter(|n| replaceable(n)) {
            let (Some(imported), Some(tested)) = (versions.get(name), calibrated.get(name)) else {
                continue;
            };
            if let [(provider, defined)] = providers(name)[..]
                && provider != observer
                && !imported.is_empty()
                && imported.is_subset(tested)
                && tested.is_subset(defined)
            {
                covered.insert(
                    name.clone(),
                    Value::object([
                        ("method", Value::str("calibrated-sole-provider")),
                        ("provider", Value::str(provider)),
                        ("certificate", sha.clone()),
                    ]),
                );
            }
        }
        let weak = weak_only(s(parsed, "raw_listing").unwrap_or_default());
        for name in uncovered.iter().filter(|n| weak.contains(*n)) {
            if !covered.contains_key(name) && providers(name).is_empty() {
                covered.insert(
                    name.clone(),
                    Value::object([
                        ("method", Value::str("unresolved-weak-import")),
                        ("certificate", sha.clone()),
                    ]),
                );
            }
        }
    }
    let remaining: Vec<String> = uncovered
        .iter()
        .filter(|n| !covered.contains_key(*n))
        .cloned()
        .collect();
    let mut fields = match parsed {
        Value::Object(m) => m.clone(),
        _ => vec![],
    };
    fields.push((
        "runtime_guard_coverage".into(),
        Value::Object(covered.into_iter().collect()),
    ));
    fields.push(("remaining_uncovered".into(), strs(remaining)));
    Value::Object(fields)
}

pub fn sample_valid(
    data: Option<&Value>,
    certificate: Option<&Value>,
    linked: &Value,
    observer_sha: Option<&str>,
    probe_sha: Option<&str>,
) -> bool {
    certificate_valid(certificate, linked, observer_sha, probe_sha)
        && data.is_some_and(|d| zero_active(d).is_ok())
}

// ---------------------------------------------------------------------------
// Optional kernel resident-set bound.

fn u16le(b: &[u8], at: usize) -> R<u16> {
    b.get(at..at + 2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .ok_or_else(|| "truncated ELF".into())
}

fn u32le(b: &[u8], at: usize) -> R<u32> {
    b.get(at..at + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        .ok_or_else(|| "truncated ELF".into())
}

fn u64le(b: &[u8], at: usize) -> R<u64> {
    b.get(at..at + 8)
        .map(|x| u64::from_le_bytes(x.try_into().unwrap()))
        .ok_or_else(|| "truncated ELF".into())
}

/// The GNU build-id note of a bounded ELF64 little-endian AArch64 module.
pub fn elf_build_id_note(binary: &[u8]) -> R<Vec<u8>> {
    if binary.len() < 64 || binary.len() > 16 * 1024 * 1024 || &binary[..6] != b"\x7fELF\x02\x01" {
        return Err("module is not a bounded ELF64 little-endian file".into());
    }
    if (u16le(binary, 16)?, u16le(binary, 18)?) != (1, 183) {
        return Err("module is not an AArch64 relocatable ELF".into());
    }
    let offset = u64le(binary, 40)? as usize;
    let (width, count, names_index) = (
        usize::from(u16le(binary, 58)?),
        usize::from(u16le(binary, 60)?),
        usize::from(u16le(binary, 62)?),
    );
    if width != 64
        || !(1..=4096).contains(&count)
        || names_index >= count
        || offset + count * width > binary.len()
    {
        return Err("invalid/extended module section table".into());
    }
    let section = |index: usize| -> R<(usize, &[u8])> {
        let at = offset + index * width;
        let name = u32le(binary, at)? as usize;
        if u32le(binary, at + 4)? == 8 {
            return Ok((name, &[]));
        }
        let (start, size) = (
            u64le(binary, at + 24)? as usize,
            u64le(binary, at + 32)? as usize,
        );
        if start.checked_add(size).is_none_or(|end| end > binary.len()) {
            return Err("module section outside file".into());
        }
        Ok((name, &binary[start..start + size]))
    };
    let (_, names) = section(names_index)?;
    if names.len() > 1024 * 1024 {
        return Err("module section-name table too large".into());
    }
    let mut notes = Vec::new();
    for index in 0..count {
        let (name, data) = section(index)?;
        if name >= names.len() {
            return Err("module section name outside table".into());
        }
        let end = names[name..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("unterminated module section name")?
            + name;
        if &names[name..end] == b".note.gnu.build-id" {
            notes.push(data);
        }
    }
    if notes.len() != 1 || notes[0].len() < 16 {
        return Err("missing/ambiguous module GNU build-id note".into());
    }
    let note = notes[0];
    let (name_size, descriptor, kind) =
        (u32le(note, 0)?, u32le(note, 4)? as usize, u32le(note, 8)?);
    if name_size != 4
        || kind != 3
        || &note[12..16] != b"GNU\0"
        || !(1..=64).contains(&descriptor)
        || note.len() != 16 + ((descriptor + 3) & !3)
    {
        return Err("malformed module GNU build-id note".into());
    }
    Ok(note.to_vec())
}

fn page_size() -> Option<i128> {
    let out = Command::new("getconf").arg("PAGESIZE").output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The reviewed kernel-bound proof still matches its pinned sources, binaries,
/// the running kernel and the loaded module's build-id.
pub fn kernel_profile_current(
    root: &Path,
    profile: &Value,
    observer_sha: Option<&str>,
    probe_sha: Option<&str>,
) -> bool {
    let check = || -> Option<bool> {
        let proof = profile.get("proof")?;
        let files = proof.get("files")?;
        let runtime = proof.get("runtime_files")?;
        let has = |v: &Value, keys: &[&str]| keys.iter().all(|k| v.get(k).is_some());
        let header = cfg!(target_os = "linux")
            && s(proof, "schema")? == "linux-rss-kernel-bound-proof-v1"
            && s(proof, "status")? == "reviewed-conditional-upper-bound"
            && py_int(proof.get("abi")) == Some(1)
            && s(proof, "kernel_release")? == kernel_release()
            && s(proof, "architecture")? == machine()
            && py_int(proof.get("page_size")).is_some_and(|p| Some(p) == page_size())
            && has(files, &KERNEL_RSS_ROLES)
            && has(runtime, &KERNEL_RUNTIME_FILES);
        if !header {
            return Some(false);
        }
        let coverage = proof.get("coverage")?;
        let complete = [
            "complete_hooks",
            "single_thread_since_constructor",
            "full_thp_disabled_before_open",
        ]
        .iter()
        .all(|k| is_true(coverage.get(k)))
            && s(coverage, "lifecycle")? == "constructor-open-to-END-ioctl";
        if !complete {
            return Some(false);
        }
        let Value::Object(pinned) = files else {
            return Some(false);
        };
        for pin in pinned
            .iter()
            .map(|(_, v)| v)
            .chain(std::iter::once(coverage.get("review_artifact")?))
        {
            if file_sha(&root.join(s(pin, "path")?)).ok().as_deref() != s(pin, "sha256") {
                return Some(false);
            }
        }
        let Value::Object(live) = runtime else {
            return Some(false);
        };
        for (path, expected) in live {
            let allowed = KERNEL_RUNTIME_FILES.contains(&path.as_str())
                || path == "/sys/module/triergon_rss_bound/srcversion";
            if !allowed || file_sha(Path::new(path)).ok().as_deref() != expected.as_str() {
                return Some(false);
            }
        }
        let pin_sha = |role: &str| files.get(role).and_then(|f| s(f, "sha256"));
        let proof_path = root.join(s(profile, "proof_path")?);
        if pin_sha("observer_binary") != observer_sha
            || pin_sha("probe_binary") != probe_sha
            || pin_sha("module_api") != pin_sha("observer_api")
            || file_sha(&proof_path).ok().as_deref() != s(profile, "proof_sha256")
            || !crate::checks::read_json(&proof_path).is_ok_and(|p| equal(&p, proof))
        {
            return Some(false);
        }
        let live_note = fs::read(KERNEL_NOTE).ok()?;
        let module = fs::read(root.join(s(files.get("module_binary")?, "path")?)).ok()?;
        Some(elf_build_id_note(&module).is_ok_and(|note| note == live_note))
    };
    check().unwrap_or(false)
}

pub fn load_kernel_profile(root: &Path, observer_sha: &str, probe_sha: &str) -> R<Value> {
    let identity = crate::checks::read_json(&root.join("target/harness/identity.json"))?;
    let pin = identity
        .get("instrumentation")
        .and_then(|i| i.get("rss_kernel_bound"))
        .ok_or("'rss_kernel_bound'")?;
    let path = s(pin, "proof_path").ok_or("'proof_path'")?;
    let profile = Value::object([
        ("proof_path", Value::str(path)),
        (
            "proof_sha256",
            pin.get("proof_sha256").cloned().unwrap_or(Value::Null),
        ),
        ("proof", crate::checks::read_json(&root.join(path))?),
    ]);
    if !kernel_profile_current(root, &profile, Some(observer_sha), Some(probe_sha)) {
        return Err(
            "kernel RSS module/source/ABI/lifecycle/live identity proof unavailable or stale"
                .into(),
        );
    }
    Ok(profile)
}

pub fn certify_kernel(
    root: &Path,
    records: &[Value],
    profile: &Value,
    observer_sha: &str,
    probe_sha: &str,
) -> R<Value> {
    if !kernel_profile_current(root, profile, Some(observer_sha), Some(probe_sha)) {
        return Err("kernel RSS parent proof is not current".into());
    }
    let expected: [(&str, i128); 3] = [
        ("default", 4 << 20),
        ("writev", 4 << 20),
        ("transient", 32 << 20),
    ];
    let mut seen = BTreeSet::new();
    for record in records {
        let case = s(record, "case").unwrap_or_default();
        let observed = record.get("observed").ok_or("'observed'")?;
        let Some(&(_, bytes)) = expected.iter().find(|e| e.0 == case) else {
            return Err("missing/duplicate/failed kernel RSS calibration".into());
        };
        if seen.contains(case)
            || s(observed, "status") != Some("measured")
            || s(observed, "instrument_sha256") != Some(observer_sha)
        {
            return Err("missing/duplicate/failed kernel RSS calibration".into());
        }
        let (data, work) = (
            observed.get("observer").ok_or("'observer'")?,
            observed.get("work").ok_or("'work'")?,
        );
        let n = |k: &str| py_int(data.get(k)).unwrap_or(-1);
        let proven = valid_receipt(data, "rss")
            && py_int(data.get("kernel_rss_requested")) == Some(1)
            && s(work, "calibration") == Some(case)
            && py_int(work.get("temporary_requested_bytes")) == Some(bytes)
            && is_true(work.get("expected_release_equal"))
            && is_false(work.get("expect_guard_invalid"))
            && n("rss_peak_observed") > n("rss_start")
            && n("kernel_rss_events") > 0
            && n("kernel_rss_positive_pages") * n("kernel_rss_page_size") >= bytes;
        if !proven {
            return Err(
                "kernel RSS calibration did not prove observed positive growth and release".into(),
            );
        }
        seen.insert(case);
    }
    if seen.len() != expected.len() {
        return Err("incomplete kernel RSS positive controls".into());
    }
    let mut result = vec![
        ("schema", Value::str("linux-rss-kernel-certificate-v1")),
        ("profile", profile.clone()),
        ("observer_sha256", Value::str(observer_sha)),
        ("probe_sha256", Value::str(probe_sha)),
        ("native_calibration_records", Value::Array(records.to_vec())),
    ];
    let sha = digest(&Value::object(result.clone()));
    result.push(("certificate_sha256", Value::str(sha)));
    Ok(Value::object(result))
}

pub fn kernel_certificate_valid(
    root: &Path,
    certificate: Option<&Value>,
    observer_sha: Option<&str>,
    probe_sha: Option<&str>,
) -> bool {
    let (Some(certificate), Some(observer), Some(probe)) = (
        certificate.filter(|c| matches!(c, Value::Object(_))),
        observer_sha,
        probe_sha,
    ) else {
        return false;
    };
    let (Some(Value::Array(records)), Some(profile)) = (
        certificate.get("native_calibration_records"),
        certificate.get("profile"),
    ) else {
        return false;
    };
    certify_kernel(root, records, profile, observer, probe)
        .is_ok_and(|again| equal(&again, certificate))
}

/// A Linux qualification certificate still describes the files on disk.
pub fn certificate_current(root: &Path, certificate: &Value) -> bool {
    let linked = certificate
        .get("linked_libraries")
        .cloned()
        .unwrap_or(Value::Object(vec![]));
    if !["candidate", "reference", "probe", "observer"]
        .iter()
        .all(|k| linked.get(k).is_some())
    {
        return false;
    }
    let (observer, probe) = (
        s(certificate, "observer_sha256"),
        s(certificate, "probe_sha256"),
    );
    if certificate
        .get("kernel_rss_required")
        .is_some_and(Value::truthy)
        && !kernel_certificate_valid(
            root,
            certificate.get("kernel_rss_certificate"),
            observer,
            probe,
        )
    {
        return false;
    }
    let Value::Object(roles) = &linked else {
        return false;
    };
    if s(certificate, "status") == Some("qualified-selected-workers")
        || certificate
            .get("runtime_guard_required")
            .is_some_and(Value::truthy)
    {
        let artifacts_ok = roles.iter().all(|(role, data)| {
            let expected = match role.as_str() {
                "candidate" | "reference" => certificate
                    .get("worker_hashes")
                    .and_then(|hashes| s(hashes, role)),
                "probe" => probe,
                "observer" => observer,
                _ => None,
            };
            data.get("inspected_artifact")
                .filter(|a| a.truthy())
                .is_some_and(|a| {
                    expected.is_some()
                        && s(a, "sha256") == expected
                        && file_sha(Path::new(s(a, "path").unwrap_or_default()))
                            .ok()
                            .as_deref()
                            == expected
                })
        });
        if !is_true(certificate.get("runtime_guard_required"))
            || !certificate_valid(
                certificate.get("runtime_guard_certificate"),
                &linked,
                observer,
                probe,
            )
            || !artifacts_ok
        {
            return false;
        }
    }
    let environment_same = loader_environment().is_ok_and(|e| {
        certificate
            .get("binding_environment")
            .is_some_and(|b| equal(b, &e))
    });
    let hashes_same = certificate
        .get("linked_library_hashes")
        .is_some_and(|h| equal(h, &linked_hashes(&linked)));
    let files_same = roles.iter().all(|(_, data)| {
        data.get("libraries")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .all(|row| {
                let real = s(row, "realpath").unwrap_or_default();
                canonical(s(row, "path").unwrap_or_default()) == real
                    && file_sha(Path::new(real)).ok().as_deref() == s(row, "sha256")
            })
    });
    environment_same && hashes_same && files_same
}
